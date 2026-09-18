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
use crate::io::{
    close_nocancel, my_close, my_close_nocancel, my_read, my_read_nocancel, my_readv,
    my_readv_nocancel, my_write, my_write_nocancel, my_writev, my_writev_nocancel, read_nocancel,
    readv_nocancel, write_nocancel, writev_nocancel,
};
use crate::process::{
    my_execve, my_fork, my_getpid, my_getppid, my_kill, my_posix_spawn, my_posix_spawnp, my_wait,
    my_wait4, my_waitpid,
};
use crate::sched::{self, my_id, State};
use crate::shared;

#[repr(C)]
struct Interpose {
    new: *const (),
    old: *const (),
}

unsafe impl Sync for Interpose {}

macro_rules! interposers {
    ($($new:ident => $old:path),* $(,)?) => {
        #[used]
        #[link_section = "__DATA,__interpose"]
        static INTERPOSERS: [Interpose; interposers!(@count $($new)*)] = [
            $(Interpose { new: $new as *const (), old: $old as *const () },)*
        ];
    };
    (@count) => { 0 };
    (@count $x:ident $($rest:ident)*) => { 1 + interposers!(@count $($rest)*) };
}

extern "C" {
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
pub extern "C" fn thread_teardown(value: *mut c_void) {
    let id = value as usize - 1;
    count(C_EXIT);
    sched::wake_all(join_key(id));
    sched::yield_baton_as(id, State::Exited, 0);
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
    if my_id().is_some() {
        sched::wake_all(m as usize);
    }
    rc
}

fn cond_enqueue(c: usize, me: usize, timed: bool) {
    sched::with(|s, _| s.cond_enqueue(me, c as u64, timed));
}

/// Returns true when signaled, false on timeout
fn cond_block(c: usize, me: usize) -> bool {
    loop {
        let done = sched::with(|s, _| {
            let t = &mut s.threads[me];
            if t.signaled {
                t.signaled = false;
                t.timed = false;
                return Some(true);
            }
            if t.timed_out {
                t.timed = false;
                t.timed_out = false;
                t.cond_key = 0;
                return Some(false);
            }
            None
        });
        if let Some(Some(signaled)) = done {
            return signaled;
        }
        sched::yield_baton(State::Blocked(c), c as u64);
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
    cond_enqueue(c as usize, me, false);
    my_pthread_mutex_unlock(m);
    cond_block(c as usize, me);
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
    cond_enqueue(c as usize, me, true);
    my_pthread_mutex_unlock(m);
    let signaled = cond_block(c as usize, me);
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

fn cond_wake(c: usize, all: bool) {
    sched::with(|s, pid| s.cond_wake(pid, c as u64, all));
}

extern "C" fn my_pthread_cond_signal(c: *mut libc::pthread_cond_t) -> c_int {
    if my_id().is_none() {
        return unsafe { libc::pthread_cond_signal(c) };
    }
    cond_wake(c as usize, false);
    0
}

extern "C" fn my_pthread_cond_broadcast(c: *mut libc::pthread_cond_t) -> c_int {
    if my_id().is_none() {
        return unsafe { libc::pthread_cond_broadcast(c) };
    }
    cond_wake(c as usize, true);
    0
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

/// Block on `addr` once. Returns false if the wait timed out.
fn futex_block(addr: *mut c_void, timed: bool) -> bool {
    let me = my_id().unwrap();
    sched::with(|s, _| {
        s.threads[me].timed = timed;
        s.threads[me].timed_out = false;
    });
    sched::yield_baton(State::Blocked(addr as usize), addr as usize as u64);
    sched::with(|s, _| {
        let t = &mut s.threads[me];
        t.timed = false;
        !std::mem::take(&mut t.timed_out)
    })
    .unwrap_or(true)
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

fn timed_out_errno() -> c_int {
    unsafe { *libc::__error() = libc::ETIMEDOUT };
    -1
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
        sched::yield_baton(State::Runnable, 0);
        return 0;
    }
    if futex_block(addr, timeout_us != 0) {
        0
    } else {
        timed_out_errno()
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
        sched::yield_baton(State::Runnable, 0);
        return 0;
    }
    if futex_block(addr, timeout_ns != 0) {
        0
    } else {
        timed_out_errno()
    }
}

extern "C" fn my_ulock_wake(op: u32, addr: *mut c_void, wake_value: u64) -> c_int {
    if my_id().is_none() {
        return unsafe { __ulock_wake(op, addr, wake_value) };
    }
    sched::wake_all(addr as usize);
    0
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
    futex_block(addr, false);
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
    if futex_block(addr, true) {
        0
    } else {
        timed_out_errno()
    }
}

extern "C" fn my_os_sync_wake_by_address_any(addr: *mut c_void, size: usize, flags: u32) -> c_int {
    if my_id().is_none() {
        return unsafe { os_sync_wake_by_address_any(addr, size, flags) };
    }
    sched::wake_all(addr as usize);
    0
}

extern "C" fn my_os_sync_wake_by_address_all(addr: *mut c_void, size: usize, flags: u32) -> c_int {
    if my_id().is_none() {
        return unsafe { os_sync_wake_by_address_all(addr, size, flags) };
    }
    sched::wake_all(addr as usize);
    0
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
        if !futex_block(sema, timeout != DISPATCH_TIME_FOREVER) {
            return 1;
        }
    }
}

extern "C" fn my_dispatch_semaphore_signal(sema: *mut c_void) -> isize {
    let rc = unsafe { dispatch_semaphore_signal(sema) };
    if my_id().is_some() {
        sched::wake_all(sema as usize);
    }
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
    sched::yield_baton(State::Runnable, 0);
    0
}

extern "C" fn my_usleep(us: u32) -> c_int {
    if my_id().is_none() {
        return unsafe { libc::usleep(us) };
    }
    count(C_YIELD);
    sched::yield_baton(State::Runnable, 0);
    0
}

extern "C" fn my_sleep(s: u32) -> u32 {
    if my_id().is_none() {
        return unsafe { libc::sleep(s) };
    }
    count(C_YIELD);
    sched::yield_baton(State::Runnable, 0);
    0
}

interposers! {
    my_pthread_create => libc::pthread_create,
    my_pthread_join => libc::pthread_join,
    my_pthread_mutex_lock => libc::pthread_mutex_lock,
    my_pthread_mutex_unlock => libc::pthread_mutex_unlock,
    my_pthread_cond_wait => libc::pthread_cond_wait,
    my_pthread_cond_timedwait => libc::pthread_cond_timedwait,
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
    my_getpid => libc::getpid,
    my_getppid => libc::getppid,
    my_kill => libc::kill,
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
