//! dyld interposers for the libSystem calls that affect the schedule.
//! Each replacement forwards to the real function unless the calling
//! thread is registered with the scheduler and the call would block, in
//! which case the block becomes a scheduler wait.
//!
//! dyld does not apply interposition to this image itself, so calling the
//! original name from here reaches the real implementation.

use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::alloc::{
    my_aligned_alloc, my_calloc, my_free, my_malloc, my_malloc_good_size, my_malloc_size,
    my_posix_memalign, my_realloc, my_valloc,
};
use crate::determinism::{
    my_arc4random, my_arc4random_buf, my_arc4random_uniform, my_cc_random_generate_bytes,
    my_clock_gettime, my_clock_gettime_nsec_np, my_getentropy, my_gettimeofday,
    my_mach_absolute_time, my_mach_continuous_time, my_time,
};
use crate::files::{
    my_flock, my_fsync, my_lseek, my_pread, my_pwrite, open_nocancel, rewrite_open_nocancel_shim,
    rewrite_open_shim, rewrite_openat_shim,
};
use crate::gcd as gcd_real;
use crate::gcd::{
    rewrite_dispatch_after_f_shim, rewrite_dispatch_after_shim, rewrite_dispatch_apply_f_shim,
    rewrite_dispatch_apply_shim, rewrite_dispatch_async_f_shim, rewrite_dispatch_async_shim,
    rewrite_dispatch_barrier_async_f_shim, rewrite_dispatch_barrier_async_shim,
    rewrite_dispatch_group_async_f_shim, rewrite_dispatch_group_async_shim,
    rewrite_dispatch_group_notify_f_shim, rewrite_dispatch_group_notify_shim,
    rewrite_dispatch_io_create_shim, rewrite_dispatch_main_shim, rewrite_dispatch_read_shim,
    rewrite_dispatch_source_create_shim, rewrite_dispatch_write_shim,
};
use crate::hostfs::{
    my_access, my_chdir, my_chmod, my_chown, my_clonefile, my_creat, my_faccessat, my_fchmodat,
    my_fstatat, my_lchown, my_link, my_lstat, my_mkdir, my_mkdirat, my_mkfifo, my_opendir,
    my_readlink, my_readlinkat, my_rename, my_renameat, my_rmdir, my_stat, my_statfs, my_symlink,
    my_truncate, my_unlink, my_unlinkat, my_utimensat, my_utimes,
};
use crate::io::{
    close_nocancel, my_close, my_close_nocancel, my_read, my_read_nocancel, my_readv,
    my_readv_nocancel, my_write, my_write_nocancel, my_writev, my_writev_nocancel, read_nocancel,
    readv_nocancel, write_nocancel, writev_nocancel,
};
use crate::kq::{my_kevent, my_kqueue};
use crate::names::{
    my_freeaddrinfo, my_freeifaddrs, my_getaddrinfo, my_gethostname, my_getifaddrs,
};
use crate::net::{
    my_accept, my_bind, my_connect, my_dup, my_dup2, my_getpeername, my_getsockname, my_getsockopt,
    my_listen, my_recv, my_recvfrom, my_recvmsg, my_send, my_sendmsg, my_sendto, my_setsockopt,
    my_shutdown, my_socket, rewrite_fcntl_shim, rewrite_ioctl_shim,
};
use crate::poll::{my_poll, my_select};
use crate::process::{
    my_execve, my_fork, my_kill, my_posix_spawn, my_posix_spawnp, my_vfork, my_wait, my_wait4,
    my_waitpid,
};
use crate::process::{rewrite_getpid_shim, rewrite_getppid_shim};
use crate::sched::{self, my_id, State};
use crate::shared;
use crate::signals::{my_sigaction, my_signal};

#[repr(C)]
struct Interpose {
    new: *const (),
    old: *const (),
}

unsafe impl Sync for Interpose {}

macro_rules! interposers {
    ($($new:ident => $old:path),* $(,)?) => {
        // Counted without recursion: the table is past the recursion limit
        const INTERPOSER_COUNT: usize = [$(stringify!($new)),*].len();
        #[used]
        #[link_section = "__DATA,__interpose"]
        static INTERPOSERS: [Interpose; INTERPOSER_COUNT] = [
            $(Interpose { new: $new as *const (), old: $old as *const () },)*
        ];
    };
}

extern "C" {
    fn pthread_cond_timedwait_relative_np(
        c: *mut libc::pthread_cond_t,
        m: *mut libc::pthread_mutex_t,
        ts: *const libc::timespec,
    ) -> c_int;
    fn __ulock_wait(op: u32, addr: *mut c_void, value: u64, timeout_us: u32) -> c_int;
    fn __ulock_wait2(op: u32, addr: *mut c_void, value: u64, timeout_ns: u64, value2: u64)
        -> c_int;
    fn __ulock_wake(op: u32, addr: *mut c_void, wake_value: u64) -> c_int;
    fn os_sync_wait_on_address(addr: *mut c_void, value: u64, size: usize, flags: u32) -> c_int;
    fn os_sync_wait_on_address_with_timeout(
        addr: *mut c_void,
        value: u64,
        size: usize,
        flags: u32,
        clockid: u32,
        timeout_ns: u64,
    ) -> c_int;
    fn os_sync_wake_by_address_any(addr: *mut c_void, size: usize, flags: u32) -> c_int;
    fn os_sync_wake_by_address_all(addr: *mut c_void, size: usize, flags: u32) -> c_int;
    fn pthread_yield_np();
    fn vfork() -> libc::pid_t;
    fn dispatch_semaphore_wait(sema: *mut c_void, timeout: u64) -> isize;
    fn dispatch_semaphore_signal(sema: *mut c_void) -> isize;
    fn CCRandomGenerateBytes(buf: *mut c_void, n: usize) -> c_int;
    fn valloc(size: usize) -> *mut c_void;
    fn clock_gettime_nsec_np(clk: libc::clockid_t) -> u64;
    fn mach_absolute_time() -> u64;
    fn mach_continuous_time() -> u64;
}

const DISPATCH_TIME_NOW: u64 = 0;
const DISPATCH_TIME_FOREVER: u64 = u64::MAX;

/// Counts of interposed calls that took the scheduler path, for the report
pub static COUNTS: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];
pub const C_CREATE: usize = 0;
pub const C_JOIN: usize = 1;
pub const C_MUTEX: usize = 2;
pub const C_COND: usize = 3;
pub const C_ULOCK: usize = 4;
pub const C_OSSYNC: usize = 5;
pub const C_YIELD: usize = 6;
pub const C_EXIT: usize = 7;
pub const C_DISPATCH: usize = 8;
pub const COUNT_NAMES: [&str; 9] = [
    "create",
    "join",
    "mutex_wait",
    "cond_wait",
    "ulock_wait",
    "os_sync_wait",
    "yield",
    "exit",
    "dispatch_wait",
];

fn count(i: usize) {
    COUNTS[i].fetch_add(1, Ordering::Relaxed);
}

/// Pseudo address joiners block on; never a real pointer
fn join_key(id: usize) -> usize {
    0x7FFF_0000_0000 + id
}

struct Start {
    f: extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
    id: usize,
}

extern "C" fn trampoline(p: *mut c_void) -> *mut c_void {
    let start = unsafe { Box::from_raw(p.cast::<Start>()) };
    sched::set_my_id(start.id);
    sched::wait_for_baton(start.id);
    (start.f)(start.arg)
}

/// Runs during the exiting thread's TSD cleanup, after dyld's thread-local
/// destructors (its key is older than ours), so guest destructors ran with
/// the baton. Marks the thread exited and hands the baton on.
/// The fourth and last round of key destructors libpthread makes
const LAST_ROUND: usize = 3;

pub extern "C" fn thread_teardown(value: *mut c_void) {
    let id = (value as usize & sched::ID_MASK) - 1;
    // Our key is older than any of the guest's, so we are called first in
    // each round of destructors, and the guest's would run after we have
    // given the baton away: guest code outside the schedule. libpthread
    // makes up to four rounds while values remain. Put ours back for all
    // but the last, and the guest's destructors run with the baton.
    let round = value as usize >> sched::ROUND_SHIFT;
    if round < LAST_ROUND {
        sched::rearm_identity(id, round + 1);
        return;
    }
    count(C_EXIT);
    // libpthread cleared our key before calling us. The join wake is still
    // this scheduled thread's, made with the baton: it must not count as a
    // wake from outside the schedule. After it, the thread is outside.
    sched::set_identity(Some(id));
    sched::wake_all(join_key(id));
    sched::set_identity(None);
    sched::forget_thread();
    sched::yield_baton_as(id, State::Exited, 0, None);
}

extern "C" fn my_pthread_create(
    t: *mut libc::pthread_t,
    attr: *const libc::pthread_attr_t,
    f: extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
) -> c_int {
    if my_id().is_none() {
        return unsafe { libc::pthread_create(t, attr, f, arg) };
    }
    count(C_CREATE);
    let id = sched::add_thread();
    let start = Box::into_raw(Box::new(Start { f, arg, id }));
    let rc = unsafe { libc::pthread_create(t, attr, trampoline, start.cast()) };
    if rc == 0 {
        let handle = unsafe { *t } as u64;
        sched::with(|s, _| s.threads[id].pthread = handle);
    } else {
        sched::with(|s, _| s.threads[id].state = shared::T_EXITED);
        drop(unsafe { Box::from_raw(start) });
    }
    rc
}

extern "C" fn my_pthread_join(t: libc::pthread_t, ret: *mut *mut c_void) -> c_int {
    if my_id().is_some() {
        count(C_JOIN);
        loop {
            let pending = sched::with(|s, pid| {
                s.find_pthread(pid, t as u64)
                    .filter(|&id| s.threads[id].state != shared::T_EXITED)
            });
            let Some(Some(id)) = pending else { break };
            sched::yield_baton(State::Blocked(join_key(id)), join_key(id) as u64);
        }
    }
    // The real join waits on a ulock the kernel wakes at thread
    // termination; that wait must not become a scheduler wait.
    sched::with_passthrough(|| unsafe { libc::pthread_join(t, ret) })
}

extern "C" fn my_pthread_mutex_lock(m: *mut libc::pthread_mutex_t) -> c_int {
    if my_id().is_none() {
        return unsafe { libc::pthread_mutex_lock(m) };
    }
    loop {
        let rc = unsafe { libc::pthread_mutex_trylock(m) };
        if rc != libc::EBUSY {
            return rc;
        }
        count(C_MUTEX);
        sched::yield_baton(State::Blocked(m as usize), m as usize as u64);
    }
}

extern "C" fn my_pthread_mutex_unlock(m: *mut libc::pthread_mutex_t) -> c_int {
    let rc = unsafe { libc::pthread_mutex_unlock(m) };
    // Also from a thread the scheduler does not run: one of ours may wait
    sched::wake_all(m as usize);
    rc
}

fn cond_enqueue(c: usize, me: usize) {
    sched::with(|s, _| s.cond_enqueue(me, c as u64));
}

/// Returns true when signaled, false when `deadline` passed first
fn cond_block(c: usize, me: usize, deadline: Option<u64>) -> bool {
    loop {
        let signaled = sched::with(|s, _| std::mem::take(&mut s.threads[me].signaled));
        if signaled == Some(true) {
            return true;
        }
        if sched::block_until(c as u64, deadline) {
            // A signal that raced the deadline still counts
            return sched::with(|s, _| {
                let t = &mut s.threads[me];
                t.cond_key = 0;
                std::mem::take(&mut t.signaled)
            }) == Some(true);
        }
    }
}

extern "C" fn my_pthread_cond_wait(
    c: *mut libc::pthread_cond_t,
    m: *mut libc::pthread_mutex_t,
) -> c_int {
    let Some(me) = my_id() else {
        return unsafe { libc::pthread_cond_wait(c, m) };
    };
    count(C_COND);
    cond_enqueue(c as usize, me);
    my_pthread_mutex_unlock(m);
    cond_block(c as usize, me, None);
    my_pthread_mutex_lock(m)
}

extern "C" fn my_pthread_cond_timedwait(
    c: *mut libc::pthread_cond_t,
    m: *mut libc::pthread_mutex_t,
    ts: *const libc::timespec,
) -> c_int {
    let Some(me) = my_id() else {
        return unsafe { libc::pthread_cond_timedwait(c, m, ts) };
    };
    count(C_COND);
    // The deadline is absolute on the (virtual) realtime clock
    let abs = unsafe {
        ((*ts).tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add((*ts).tv_nsec as u64)
    };
    let deadline = abs.saturating_sub(crate::determinism::REALTIME_BASE_NS);
    cond_wait_until(c, m, me, deadline)
}

/// What Rust's `Condvar::wait_timeout` calls on this platform.
extern "C" fn my_pthread_cond_timedwait_relative_np(
    c: *mut libc::pthread_cond_t,
    m: *mut libc::pthread_mutex_t,
    ts: *const libc::timespec,
) -> c_int {
    let Some(me) = my_id() else {
        return unsafe { pthread_cond_timedwait_relative_np(c, m, ts) };
    };
    count(C_COND);
    let rel = unsafe {
        ((*ts).tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add((*ts).tv_nsec as u64)
    };
    cond_wait_until(c, m, me, sched::now().saturating_add(rel))
}

fn cond_wait_until(
    c: *mut libc::pthread_cond_t,
    m: *mut libc::pthread_mutex_t,
    me: usize,
    deadline: u64,
) -> c_int {
    cond_enqueue(c as usize, me);
    my_pthread_mutex_unlock(m);
    let signaled = cond_block(c as usize, me, Some(deadline));
    let rc = my_pthread_mutex_lock(m);
    if rc != 0 {
        return rc;
    }
    if signaled {
        0
    } else {
        libc::ETIMEDOUT
    }
}

// A contended real rwlock would sleep in the kernel with the baton held.
// Try, and wait in the scheduler for an unlock instead. Writers are not
// preferred over readers; who gets the lock next is the scheduler's draw.

extern "C" fn my_pthread_rwlock_rdlock(l: *mut libc::pthread_rwlock_t) -> c_int {
    if my_id().is_none() {
        return unsafe { libc::pthread_rwlock_rdlock(l) };
    }
    loop {
        let rc = unsafe { libc::pthread_rwlock_tryrdlock(l) };
        if rc != libc::EBUSY {
            return rc;
        }
        count(C_MUTEX);
        sched::yield_baton(State::Blocked(l as usize), l as usize as u64);
    }
}

extern "C" fn my_pthread_rwlock_wrlock(l: *mut libc::pthread_rwlock_t) -> c_int {
    if my_id().is_none() {
        return unsafe { libc::pthread_rwlock_wrlock(l) };
    }
    loop {
        let rc = unsafe { libc::pthread_rwlock_trywrlock(l) };
        if rc != libc::EBUSY {
            return rc;
        }
        count(C_MUTEX);
        sched::yield_baton(State::Blocked(l as usize), l as usize as u64);
    }
}

extern "C" fn my_pthread_rwlock_unlock(l: *mut libc::pthread_rwlock_t) -> c_int {
    let rc = unsafe { libc::pthread_rwlock_unlock(l) };
    sched::wake_all(l as usize);
    rc
}

fn cond_wake(c: usize, all: bool) {
    sched::with(|s, pid| s.cond_wake(pid, c as u64, all));
}

// Wakes go to both worlds: a scheduled waiter is parked in the scheduler,
// a GCD worker sleeps in the kernel, and the waker cannot know which it has.

extern "C" fn my_pthread_cond_signal(c: *mut libc::pthread_cond_t) -> c_int {
    cond_wake(c as usize, false);
    unsafe { libc::pthread_cond_signal(c) }
}

extern "C" fn my_pthread_cond_broadcast(c: *mut libc::pthread_cond_t) -> c_int {
    cond_wake(c as usize, true);
    unsafe { libc::pthread_cond_broadcast(c) }
}

/// Compare `*addr` with `value` at the width the ulock or `os_sync` op implies
fn value_matches(addr: *mut c_void, value: u64, wide: bool) -> bool {
    unsafe {
        if wide {
            addr.cast::<u64>().read_volatile() == value
        } else {
            addr.cast::<u32>().read_volatile() == value as u32
        }
    }
}

/// Block on `addr` once, for at most `timeout_ns` of virtual time (0:
/// forever). Returns false if the wait timed out.
fn futex_block(addr: *mut c_void, timeout_ns: u64) -> bool {
    let deadline = (timeout_ns != 0).then(|| sched::now().saturating_add(timeout_ns));
    !sched::block_until(addr as usize as u64, deadline)
}

/// Sleep for `ns` of virtual time: a wait nothing but the deadline ends.
fn sleep_ns(ns: u64) {
    let deadline = sched::now().saturating_add(ns);
    while !sched::block_until(shared::SLEEP_KEY, Some(deadline)) {}
}

fn ulock_is_wide(op: u32) -> bool {
    matches!(op & 0xFF, 4..=6)
}

/// `os_unfair_lock` waits (`UL_UNFAIR_LOCK`, `UL_UNFAIR_LOCK64_SHARED`) are
/// woken only if the kernel set the waiter bit while the waiter slept in
/// the kernel; since ours never do, the unlock would not call
/// `__ulock_wake`. Those waits yield and retry instead of parking.
fn ulock_is_unfair(op: u32) -> bool {
    matches!(op & 0xFF, 2 | 4)
}

/// The lock word of an unfair lock (and of libdispatch's `dispatch_once`
/// gate) holds its owner's Mach thread port above two flag bits. When the
/// owner is a thread the scheduler does not run, yielding gets nowhere and
/// burns virtual time at a rate that depends on real time: the wait has to
/// be the real one, which that thread's unlock will end.
fn unfair_owner(value: u64) -> Option<usize> {
    let owner = (value as u32) & !3;
    if owner == 0 {
        return None;
    }
    sched::scheduled_thread(owner).or_else(|| sched::scheduled_thread(owner | 3))
}

/// A contended unfair lock. Who holds it decides what the wait is:
/// - a thread outside the schedule: the kernel's wait, which its unlock ends;
/// - one of ours that is parked: it holds the lock until it runs again, so
///   the baton goes on;
/// - one of ours that is running in real time without the baton (starting
///   up, or between a hand-off and its park, inside the system libraries):
///   it lets go by itself in a moment. Whether we caught it holding the
///   lock is real-time luck and must not show in the schedule, so that is
///   waited out in the kernel as well.
fn unfair_wait(op: u32, addr: *mut c_void, value: u64, real: impl Fn() -> c_int) -> c_int {
    const LOOK_AGAIN_NS: u64 = 1_000_000;
    let wide = ulock_is_wide(op);
    // Only the baton holder's waits are the schedule's business. Another of
    // our threads gets here from the system libraries (the allocator's own
    // lock, say) while it starts up or after it has handed the baton on.
    if !sched::baton_is_mine() {
        return real();
    }
    loop {
        match unfair_owner(value) {
            None => return real(),
            Some(owner) if sched::is_parked(owner) => {
                // Looked at in this order: it may have let go and parked
                // since the caller read the lock word, but parked it cannot
                // let go, so a word that still names it is a lock it holds
                if value_matches(addr, value, wide) {
                    sched::yield_baton(State::Runnable, 0);
                }
                return 0;
            }
            Some(_) => {
                let rc = unsafe { __ulock_wait2(op, addr, value, LOOK_AGAIN_NS, 0) };
                if !value_matches(addr, value, wide) {
                    // Not 0 when the kernel says others still wait: the
                    // caller's unlock must then wake them
                    return rc.max(0);
                }
            }
        }
    }
}

extern "C" fn my_ulock_wait(op: u32, addr: *mut c_void, value: u64, timeout_us: u32) -> c_int {
    if my_id().is_none() {
        return unsafe { __ulock_wait(op, addr, value, timeout_us) };
    }
    count(C_ULOCK);
    if !value_matches(addr, value, ulock_is_wide(op)) {
        return 0;
    }
    if ulock_is_unfair(op) {
        return unfair_wait(op, addr, value, || unsafe {
            __ulock_wait(op, addr, value, timeout_us)
        });
    }
    if futex_block(addr, u64::from(timeout_us) * 1000) {
        0
    } else {
        crate::errno::fail(libc::ETIMEDOUT)
    }
}

extern "C" fn my_ulock_wait2(
    op: u32,
    addr: *mut c_void,
    value: u64,
    timeout_ns: u64,
    value2: u64,
) -> c_int {
    if my_id().is_none() {
        return unsafe { __ulock_wait2(op, addr, value, timeout_ns, value2) };
    }
    count(C_ULOCK);
    if !value_matches(addr, value, ulock_is_wide(op)) {
        return 0;
    }
    if ulock_is_unfair(op) {
        return unfair_wait(op, addr, value, || unsafe {
            __ulock_wait2(op, addr, value, timeout_ns, value2)
        });
    }
    if futex_block(addr, timeout_ns) {
        0
    } else {
        crate::errno::fail(libc::ETIMEDOUT)
    }
}

extern "C" fn my_ulock_wake(op: u32, addr: *mut c_void, wake_value: u64) -> c_int {
    let ours = sched::wake_all(addr as usize);
    let rc = unsafe { __ulock_wake(op, addr, wake_value) };
    // "Nobody was waiting" is only true if neither world had a waiter
    if ours > 0 {
        0
    } else {
        rc
    }
}

extern "C" fn my_os_sync_wait_on_address(
    addr: *mut c_void,
    value: u64,
    size: usize,
    flags: u32,
) -> c_int {
    if my_id().is_none() {
        return unsafe { os_sync_wait_on_address(addr, value, size, flags) };
    }
    count(C_OSSYNC);
    if !value_matches(addr, value, size == 8) {
        return 0;
    }
    futex_block(addr, 0);
    0
}

extern "C" fn my_os_sync_wait_on_address_with_timeout(
    addr: *mut c_void,
    value: u64,
    size: usize,
    flags: u32,
    clockid: u32,
    timeout_ns: u64,
) -> c_int {
    if my_id().is_none() {
        return unsafe {
            os_sync_wait_on_address_with_timeout(addr, value, size, flags, clockid, timeout_ns)
        };
    }
    count(C_OSSYNC);
    if !value_matches(addr, value, size == 8) {
        return 0;
    }
    if futex_block(addr, timeout_ns.max(1)) {
        0
    } else {
        crate::errno::fail(libc::ETIMEDOUT)
    }
}

extern "C" fn my_os_sync_wake_by_address_any(addr: *mut c_void, size: usize, flags: u32) -> c_int {
    let ours = sched::wake_all(addr as usize);
    let rc = sched::with_passthrough(|| unsafe { os_sync_wake_by_address_any(addr, size, flags) });
    if ours > 0 {
        0
    } else {
        rc
    }
}

extern "C" fn my_os_sync_wake_by_address_all(addr: *mut c_void, size: usize, flags: u32) -> c_int {
    let ours = sched::wake_all(addr as usize);
    let rc = sched::with_passthrough(|| unsafe { os_sync_wake_by_address_all(addr, size, flags) });
    if ours > 0 {
        0
    } else {
        rc
    }
}

/// How long a `dispatch_time_t` deadline is from now, in nanoseconds (0:
/// forever). libdispatch computed it from a clock read of its own: through
/// our interposers (the virtual clock) or inline from the commpage (the
/// real one), depending on the path. The two clocks are far apart, so the
/// distance is taken on both and the smaller one that is not in the past
/// names the clock it came from. A real-clock distance is rounded to a
/// millisecond, which absorbs the time since libdispatch's read.
fn dispatch_timeout_ns(timeout: u64) -> u64 {
    const MS: i64 = 1_000_000;
    if timeout == DISPATCH_TIME_FOREVER {
        return 0;
    }
    let virtual_now = sched::now() as i64;
    let (deadline, real_now, virtual_now) = if (timeout as i64) < 0 {
        // Wall time, encoded as minus nanoseconds since the epoch
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &raw mut ts) };
        (
            (timeout as i64).wrapping_neg(),
            ts.tv_sec * 1_000_000_000 + ts.tv_nsec,
            crate::determinism::REALTIME_BASE_NS as i64 + virtual_now,
        )
    } else {
        // Mach absolute ticks; 125/3 ns each on Apple Silicon
        (
            (timeout as i64).saturating_mul(125) / 3,
            (unsafe { mach_absolute_time() } as i64).saturating_mul(125) / 3,
            crate::determinism::MONOTONIC_BASE_NS as i64 + virtual_now,
        )
    };
    let on_real = deadline.saturating_sub(real_now);
    let on_virtual = deadline.saturating_sub(virtual_now);
    let ns = match (on_real >= -MS, on_virtual >= -MS) {
        (true, true) if on_virtual <= on_real => on_virtual,
        (true, _) => (on_real + MS / 2) / MS * MS,
        (false, true) => on_virtual,
        (false, false) => 0,
    };
    ns.max(1) as u64
}

/// Rust's `Thread::park` sits on one of these. A zero timeout is a
/// try-wait, which lets the count live in libdispatch while the blocking
/// moves into the scheduler.
extern "C" fn my_dispatch_semaphore_wait(sema: *mut c_void, timeout: u64) -> isize {
    if my_id().is_none() {
        return unsafe { dispatch_semaphore_wait(sema, timeout) };
    }
    loop {
        if unsafe { dispatch_semaphore_wait(sema, DISPATCH_TIME_NOW) } == 0 {
            return 0;
        }
        count(C_DISPATCH);
        if !futex_block(sema, dispatch_timeout_ns(timeout)) {
            return 1;
        }
    }
}

extern "C" fn my_dispatch_semaphore_signal(sema: *mut c_void) -> isize {
    let rc = unsafe { dispatch_semaphore_signal(sema) };
    sched::wake_all(sema as usize);
    rc
}

extern "C" fn my_sched_yield() -> c_int {
    if my_id().is_none() {
        return unsafe { libc::sched_yield() };
    }
    count(C_YIELD);
    sched::yield_baton(State::Runnable, 0);
    0
}

extern "C" fn my_pthread_yield_np() {
    if my_id().is_none() {
        return unsafe { pthread_yield_np() };
    }
    count(C_YIELD);
    sched::yield_baton(State::Runnable, 0);
}

extern "C" fn my_nanosleep(req: *const libc::timespec, rem: *mut libc::timespec) -> c_int {
    if my_id().is_none() {
        return unsafe { libc::nanosleep(req, rem) };
    }
    count(C_YIELD);
    let ns = unsafe {
        ((*req).tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add((*req).tv_nsec as u64)
    };
    sleep_ns(ns);
    if !rem.is_null() {
        unsafe {
            (*rem).tv_sec = 0;
            (*rem).tv_nsec = 0;
        }
    }
    0
}

extern "C" fn my_usleep(us: u32) -> c_int {
    if my_id().is_none() {
        return unsafe { libc::usleep(us) };
    }
    count(C_YIELD);
    sleep_ns(u64::from(us) * 1000);
    0
}

extern "C" fn my_sleep(s: u32) -> u32 {
    if my_id().is_none() {
        return unsafe { libc::sleep(s) };
    }
    count(C_YIELD);
    sleep_ns(u64::from(s) * 1_000_000_000);
    0
}

interposers! {
    my_pthread_create => libc::pthread_create,
    my_pthread_join => libc::pthread_join,
    my_pthread_mutex_lock => libc::pthread_mutex_lock,
    my_pthread_mutex_unlock => libc::pthread_mutex_unlock,
    my_pthread_cond_wait => libc::pthread_cond_wait,
    my_pthread_cond_timedwait => libc::pthread_cond_timedwait,
    my_pthread_cond_timedwait_relative_np => pthread_cond_timedwait_relative_np,
    my_pthread_rwlock_rdlock => libc::pthread_rwlock_rdlock,
    my_pthread_rwlock_wrlock => libc::pthread_rwlock_wrlock,
    my_pthread_rwlock_unlock => libc::pthread_rwlock_unlock,
    my_pthread_cond_signal => libc::pthread_cond_signal,
    my_pthread_cond_broadcast => libc::pthread_cond_broadcast,
    my_ulock_wait => __ulock_wait,
    my_ulock_wait2 => __ulock_wait2,
    my_ulock_wake => __ulock_wake,
    my_os_sync_wait_on_address => os_sync_wait_on_address,
    my_os_sync_wait_on_address_with_timeout => os_sync_wait_on_address_with_timeout,
    my_os_sync_wake_by_address_any => os_sync_wake_by_address_any,
    my_os_sync_wake_by_address_all => os_sync_wake_by_address_all,
    my_dispatch_semaphore_wait => dispatch_semaphore_wait,
    my_dispatch_semaphore_signal => dispatch_semaphore_signal,
    my_sched_yield => libc::sched_yield,
    my_pthread_yield_np => pthread_yield_np,
    my_nanosleep => libc::nanosleep,
    my_usleep => libc::usleep,
    my_sleep => libc::sleep,
    my_posix_spawn => libc::posix_spawn,
    my_posix_spawnp => libc::posix_spawnp,
    my_fork => libc::fork,
    my_execve => libc::execve,
    my_waitpid => libc::waitpid,
    my_wait4 => libc::wait4,
    my_wait => libc::wait,
    rewrite_getpid_shim => libc::getpid,
    rewrite_getppid_shim => libc::getppid,
    my_kill => libc::kill,
    my_sigaction => libc::sigaction,
    my_signal => libc::signal,
    my_read => libc::read,
    my_read_nocancel => read_nocancel,
    my_readv => libc::readv,
    my_readv_nocancel => readv_nocancel,
    my_write => libc::write,
    my_write_nocancel => write_nocancel,
    my_writev => libc::writev,
    my_writev_nocancel => writev_nocancel,
    my_close => libc::close,
    my_close_nocancel => close_nocancel,
    my_socket => libc::socket,
    my_bind => libc::bind,
    my_listen => libc::listen,
    my_connect => libc::connect,
    my_accept => libc::accept,
    my_send => libc::send,
    my_recv => libc::recv,
    my_sendto => libc::sendto,
    my_recvfrom => libc::recvfrom,
    my_shutdown => libc::shutdown,
    my_getsockname => libc::getsockname,
    my_getpeername => libc::getpeername,
    my_setsockopt => libc::setsockopt,
    my_getsockopt => libc::getsockopt,
    my_dup => libc::dup,
    my_dup2 => libc::dup2,
    my_gethostname => libc::gethostname,
    my_getaddrinfo => libc::getaddrinfo,
    my_freeaddrinfo => libc::freeaddrinfo,
    my_getifaddrs => libc::getifaddrs,
    my_freeifaddrs => libc::freeifaddrs,
    my_flock => libc::flock,
    my_pread => libc::pread,
    my_pwrite => libc::pwrite,
    my_lseek => libc::lseek,
    my_fsync => libc::fsync,
    rewrite_open_shim => libc::open,
    rewrite_open_nocancel_shim => open_nocancel,
    rewrite_openat_shim => libc::openat,
    my_stat => libc::stat,
    my_lstat => libc::lstat,
    my_fstatat => libc::fstatat,
    my_access => libc::access,
    my_mkdir => libc::mkdir,
    my_mkdirat => libc::mkdirat,
    my_rmdir => libc::rmdir,
    my_unlink => libc::unlink,
    my_unlinkat => libc::unlinkat,
    my_rename => libc::rename,
    my_link => libc::link,
    my_symlink => libc::symlink,
    my_readlink => libc::readlink,
    my_chdir => libc::chdir,
    my_truncate => libc::truncate,
    my_chmod => libc::chmod,
    my_chown => libc::chown,
    my_utimes => libc::utimes,
    my_mkfifo => libc::mkfifo,
    my_creat => libc::creat,
    my_opendir => libc::opendir,
    my_renameat => libc::renameat,
    my_faccessat => libc::faccessat,
    my_readlinkat => libc::readlinkat,
    my_fchmodat => libc::fchmodat,
    my_utimensat => libc::utimensat,
    my_clonefile => libc::clonefile,
    my_statfs => libc::statfs,
    my_lchown => libc::lchown,
    my_vfork => vfork,
    rewrite_dispatch_async_shim => gcd_real::dispatch_async,
    rewrite_dispatch_async_f_shim => gcd_real::dispatch_async_f,
    rewrite_dispatch_after_shim => gcd_real::dispatch_after,
    rewrite_dispatch_after_f_shim => gcd_real::dispatch_after_f,
    rewrite_dispatch_apply_shim => gcd_real::dispatch_apply,
    rewrite_dispatch_apply_f_shim => gcd_real::dispatch_apply_f,
    rewrite_dispatch_group_async_shim => gcd_real::dispatch_group_async,
    rewrite_dispatch_group_async_f_shim => gcd_real::dispatch_group_async_f,
    rewrite_dispatch_barrier_async_shim => gcd_real::dispatch_barrier_async,
    rewrite_dispatch_barrier_async_f_shim => gcd_real::dispatch_barrier_async_f,
    rewrite_dispatch_group_notify_shim => gcd_real::dispatch_group_notify,
    rewrite_dispatch_group_notify_f_shim => gcd_real::dispatch_group_notify_f,
    rewrite_dispatch_source_create_shim => gcd_real::dispatch_source_create,
    rewrite_dispatch_main_shim => gcd_real::dispatch_main,
    rewrite_dispatch_read_shim => gcd_real::dispatch_read,
    rewrite_dispatch_write_shim => gcd_real::dispatch_write,
    rewrite_dispatch_io_create_shim => gcd_real::dispatch_io_create,
    my_kevent => libc::kevent,
    my_kqueue => libc::kqueue,
    my_poll => libc::poll,
    my_select => libc::select,
    my_sendmsg => libc::sendmsg,
    my_recvmsg => libc::recvmsg,
    rewrite_fcntl_shim => libc::fcntl,
    rewrite_ioctl_shim => libc::ioctl,
    my_malloc => libc::malloc,
    my_calloc => libc::calloc,
    my_free => libc::free,
    my_realloc => libc::realloc,
    my_posix_memalign => libc::posix_memalign,
    my_aligned_alloc => libc::aligned_alloc,
    my_valloc => valloc,
    my_malloc_size => libc::malloc_size,
    my_malloc_good_size => libc::malloc_good_size,
    my_arc4random => libc::arc4random,
    my_arc4random_uniform => libc::arc4random_uniform,
    my_arc4random_buf => libc::arc4random_buf,
    my_getentropy => libc::getentropy,
    my_cc_random_generate_bytes => CCRandomGenerateBytes,
    my_clock_gettime => libc::clock_gettime,
    my_clock_gettime_nsec_np => clock_gettime_nsec_np,
    my_gettimeofday => libc::gettimeofday,
    my_time => libc::time,
    my_mach_absolute_time => mach_absolute_time,
    my_mach_continuous_time => mach_continuous_time,
}
