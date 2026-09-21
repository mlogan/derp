//! Baton scheduler. Every guest thread in every process of the run is a
//! real pthread parked on its word in the shared state; exactly one holds
//! the baton and runs guest code. Switches happen only at quantum expiry
//! (from the stubs) and at the interposed blocking primitives, so the
//! interleaving is a function of the seed.

use std::ffi::c_void;
use std::fmt::Write;
use std::sync::atomic::{AtomicI64, AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use crate::shared::{self, Handoff, Shared};
use crate::spin::SpinLock;
use crate::stubdata::Info;

pub struct Config {
    pub seed: u64,
    /// Bytes of address space for the guest's heap
    pub heap_size: usize,
    pub quantum_lo: u32,
    pub quantum_hi: u32,
}

impl Config {
    pub fn from_env() -> Self {
        let seed = std::env::var("REWRITE_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let (mut lo, mut hi) = (1000, 10000);
        if let Ok(q) = std::env::var("REWRITE_QUANTUM") {
            if let Some((a, b)) = q.split_once("..") {
                if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
                    if a >= 1 && b >= a {
                        (lo, hi) = (a, b);
                    }
                }
            }
        }
        let heap_size = std::env::var("REWRITE_HEAP")
            .ok()
            .and_then(|v| usize::from_str_radix(&v, 16).ok())
            .unwrap_or(crate::alloc::DEFAULT_SIZE);
        Config {
            seed,
            heap_size,
            quantum_lo: lo,
            quantum_hi: hi,
        }
    }
}

/// What the calling thread becomes when it gives up the baton
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Runnable,
    /// Waiting for a wake on this key (an address or pseudo address in
    /// this process)
    Blocked(usize),
    Exited,
}

static SHARED: AtomicPtr<Shared> = AtomicPtr::new(std::ptr::null_mut());
/// This process's index in the shared process table
static PID: AtomicU32 = AtomicU32::new(0);
static INFO: SpinLock<Option<Info>> = SpinLock::new(None);
/// Hooks are attributed to the process that consumed them:
/// `LAST_QUANTUM` is what this process last saw in the counter and
/// `HOOKS` what it consumed before that.
static LAST_QUANTUM: AtomicI64 = AtomicI64::new(0);
static HOOKS: AtomicI64 = AtomicI64::new(0);

/// The thread's scheduler id lives in a pthread key (value `id + 1`) rather
/// than a Rust thread-local: the key's destructor is the thread's teardown
/// hook and receives the id even after dyld has torn down TLV storage.
static ID_KEY: AtomicUsize = AtomicUsize::new(usize::MAX);

thread_local! {
    /// Depth of pass-through sections: while positive, interposed calls on
    /// this thread go straight to libSystem (used around calls that block
    /// on something only the kernel wakes, such as the real `pthread_join`).
    static RAW: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

pub fn with_passthrough<T>(f: impl FnOnce() -> T) -> T {
    RAW.with(|r| r.set(r.get() + 1));
    let out = f();
    RAW.with(|r| r.set(r.get() - 1));
    out
}

pub fn my_id() -> Option<usize> {
    let key = ID_KEY.load(Ordering::Relaxed);
    if key == usize::MAX || RAW.with(std::cell::Cell::get) > 0 {
        return None;
    }
    let v = unsafe { libc::pthread_getspecific(key as libc::pthread_key_t) } as usize;
    (v != 0).then(|| (v & ID_MASK) - 1)
}

/// The key's value is the id plus one; above it, how many rounds of key
/// destructors the exiting thread has been through (`thread_teardown`).
pub const ID_MASK: usize = 0xFFFF_FFFF;
pub const ROUND_SHIFT: u32 = 32;

/// Keep this thread's identity for another round of key destructors.
pub fn rearm_identity(id: usize, round: usize) {
    let key = ID_KEY.load(Ordering::Relaxed) as libc::pthread_key_t;
    let value = (id + 1) | (round << ROUND_SHIFT);
    unsafe { libc::pthread_setspecific(key, value as *const c_void) };
}

/// `my_id().is_some()` for the allocator, which must not touch a Rust
/// thread-local: dyld allocates those with `malloc`.
pub fn on_scheduled_thread() -> bool {
    let key = ID_KEY.load(Ordering::Relaxed);
    key != usize::MAX && !unsafe { libc::pthread_getspecific(key as libc::pthread_key_t) }.is_null()
}

/// A `timespec` as nanoseconds, saturating.
///
/// # Safety
/// `ts` must point to a valid `timespec`.
pub unsafe fn timespec_ns(ts: *const libc::timespec) -> u64 {
    ((*ts).tv_sec.max(0) as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add((*ts).tv_nsec.max(0) as u64)
}

/// A run seed made one process's own: every per-process stream starts here.
pub fn process_seed(seed: u64, proc_index: u32) -> u64 {
    seed.wrapping_add(u64::from(proc_index).wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

pub fn set_my_id(id: usize) {
    set_identity(Some(id));
    let port = unsafe { pthread_mach_thread_np(libc::pthread_self()) };
    PORTS.lock().push((port, id));
}

/// Mach ports of the threads the scheduler runs in this process. The rest
/// (GCD workers, which the kernel creates without `pthread_create`) run
/// outside the baton; what they do is input, like the clock once was.
static PORTS: SpinLock<Vec<(u32, usize)>> = SpinLock::new(Vec::new());

extern "C" {
    fn pthread_mach_thread_np(t: libc::pthread_t) -> u32;
    fn task_threads(task: u32, list: *mut *mut u32, count: *mut u32) -> i32;
    fn vm_deallocate(task: u32, addr: usize, size: usize) -> i32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
}

/// The scheduler's id of the thread with this Mach port, if it runs it.
pub fn scheduled_thread(port: u32) -> Option<usize> {
    PORTS
        .lock()
        .iter()
        .find(|&&(p, _)| p == port)
        .map(|&(_, id)| id)
}

/// Seed bisection reseeds every random stream at a virtual time. The
/// scheduler's own are swapped in the shared state; a process's streams
/// (heap layout, entropy) are its own, and each asks here before it draws.
/// `done` is the stream's note that it has switched. The answer is the
/// process's own; each stream mixes in its constant.
pub fn reseed_once(done: &std::sync::atomic::AtomicBool) -> Option<u64> {
    if done.load(Ordering::Relaxed) {
        return None;
    }
    let with = shared()?.reseeded_with()?;
    done.store(true, Ordering::Relaxed);
    Some(process_seed(with, pid()))
}

/// Whether thread `id` is parked, as opposed to running in real time
/// without the baton (starting up, or between a hand-off and its park).
pub fn is_parked(id: usize) -> bool {
    shared().is_some_and(|sh| sh.is_parked(id))
}

/// Set or clear this thread's scheduler id without touching the port list.
/// A value left set when a key destructor returns would run it again.
pub fn set_identity(id: Option<usize>) {
    let key = ID_KEY.load(Ordering::Relaxed) as libc::pthread_key_t;
    let value = id.map_or(std::ptr::null(), |id| (id + 1) as *const c_void);
    unsafe { libc::pthread_setspecific(key, value) };
}

pub fn forget_thread() {
    let port = unsafe { pthread_mach_thread_np(libc::pthread_self()) };
    PORTS.lock().retain(|&(p, _)| p != port);
}

/// Whether this process has threads the scheduler does not run.
fn has_outside_threads() -> bool {
    let mut list: *mut u32 = std::ptr::null_mut();
    let mut count = 0u32;
    if unsafe { task_threads(mach_task_self_, &raw mut list, &raw mut count) } != 0 {
        return false;
    }
    unsafe {
        // `task_threads` hands out a send right per thread
        for i in 0..count as usize {
            mach_port_deallocate(mach_task_self_, *list.add(i));
        }
        vm_deallocate(
            mach_task_self_,
            list as usize,
            count as usize * std::mem::size_of::<u32>(),
        );
    }
    count as usize > PORTS.lock().len()
}

pub fn pid() -> u32 {
    PID.load(Ordering::Relaxed)
}

fn shared() -> Option<&'static Shared> {
    unsafe { SHARED.load(Ordering::Relaxed).as_ref() }
}

/// Run `f` on the locked scheduler state. `f` must not block or allocate.
/// None outside a run, and from a signal handler that interrupted this
/// thread inside the lock: the state is mid-update then and not for use.
pub fn with<R>(f: impl FnOnce(&mut shared::State, u32) -> R) -> Option<R> {
    let sh = shared()?;
    let mut s = sh.lock_unless_reentrant()?;
    Some(f(&mut s, pid()))
}

/// `with` for a wake, which a handler on the lock's holder leaves to the
/// holder instead of dropping.
fn with_or_defer(key: u64, f: impl FnOnce(&mut shared::State, u32) -> usize) -> usize {
    let Some(sh) = shared() else { return 0 };
    if let Some(mut s) = sh.lock_unless_reentrant() {
        f(&mut s, pid())
    } else {
        sh.defer_wake(pid(), key);
        0
    }
}

/// Make every thread of this process blocked on `addr` runnable; returns
/// how many there were. Safe from a thread the scheduler does not run.
/// `wake_io`, deferred like `wake_all` when a handler cannot make it.
pub fn wake_io() {
    let outside = !on_scheduled_thread();
    with_or_defer(shared::DEFERRED_IO, |s, pid| {
        if outside {
            note_outside_wake(s, pid);
        }
        s.wake_io();
        0
    });
}

pub fn wake_all(addr: usize) -> usize {
    let outside = !on_scheduled_thread();
    let woken = with_or_defer(addr as u64, |s, pid| {
        if outside {
            note_outside_wake(s, pid);
        }
        s.wake_all(pid, addr as u64)
    });
    if woken > 0 && outside {
        OUTSIDE_WAKES.fetch_add(woken as u64, Ordering::Relaxed);
    }
    woken
}

/// A wake from a thread the scheduler does not run: see
/// `ProcRec::outside_wakes`.
pub fn note_outside_wake(s: &mut shared::State, pid: u32) {
    let p = &mut s.procs[pid as usize];
    p.outside_wakes += 1;
    p.has_outside_threads = true;
}

/// Scheduled threads made runnable by a thread the scheduler does not run.
/// When that happens depends on real time, so a run where this is not zero
/// had its schedule influenced from outside.
pub static OUTSIDE_WAKES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

extern "C" {
    fn rewrite_scheduler_yield();
}

/// Fold the hooks consumed from the current quantum into `HOOKS`.
fn settle_hooks() {
    if let Some(sh) = shared() {
        let remaining = unsafe { sh.counter().read() };
        let last = LAST_QUANTUM.swap(remaining, Ordering::Relaxed);
        HOOKS.fetch_add(last - remaining, Ordering::Relaxed);
    }
}

fn install_quantum(q: i64) {
    if let Some(sh) = shared() {
        unsafe { sh.counter().write(q) };
        LAST_QUANTUM.store(q, Ordering::Relaxed);
    }
}

extern "C" {
    static mach_task_self_: u32;
    fn mach_vm_allocate(task: u32, addr: *mut u64, size: u64, flags: i32) -> i32;
}

/// Set up the fixed region the stubs address: this process's private page
/// at `STUB_BASE`, then `len` bytes of `fd` (or of anonymous memory for -1).
/// The stubs hard-code the address, so nothing else will do. An `mmap` hint
/// is not enough: the kernel ignores it now and then. A fixed
/// `mach_vm_allocate` fails instead of replacing what is there, so mapping
/// over that reservation with `MAP_FIXED` cannot clobber anything.
fn map_region(fd: i32, len: usize) -> *mut Shared {
    let mut addr = shared::STUB_BASE as u64;
    let total = (shared::PRIVATE_SIZE + len) as u64;
    if unsafe { mach_vm_allocate(mach_task_self_, &raw mut addr, total, 0) } != 0 {
        fatal("the fixed scheduler region is occupied in this process");
    }
    let map = |at: usize, len: usize, flags: i32, fd: i32| {
        let p = unsafe {
            libc::mmap(
                at as *mut c_void,
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                flags | libc::MAP_FIXED,
                fd,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            fatal("mapping the fixed scheduler region failed");
        }
    };
    map(
        shared::STUB_BASE,
        shared::PRIVATE_SIZE,
        libc::MAP_PRIVATE | libc::MAP_ANON,
        -1,
    );
    let state_flags = if fd < 0 {
        libc::MAP_SHARED | libc::MAP_ANON
    } else {
        libc::MAP_SHARED
    };
    map(shared::MAP_ADDR, len, state_flags, fd);
    shared::MAP_ADDR as *mut Shared
}

fn map_shared_file(path: &str) -> Option<(*mut Shared, usize)> {
    let cpath = std::ffi::CString::new(path).ok()?;
    unsafe {
        let fd = libc::open(cpath.as_ptr(), libc::O_RDWR);
        if fd < 0 {
            return None;
        }
        let mut st: libc::stat = std::mem::zeroed();
        let mapped = (libc::fstat(fd, &raw mut st) == 0).then(|| {
            let len = st.st_size as usize;
            (map_region(fd, len), len)
        });
        libc::close(fd);
        mapped
    }
}

fn scheduler_slot() -> *mut usize {
    (shared::STUB_BASE + shared::SLOT_OFFSET as usize) as *mut usize
}

pub fn fatal(msg: &str) -> ! {
    crate::report::log(msg);
    std::process::abort();
}

/// Attach to the state the launcher prepared; it registered this process
/// and its main thread before spawning us.
fn join_run(path: &str) -> (&'static Shared, usize) {
    let Some((mem, len)) = map_shared_file(path) else {
        fatal("cannot map the run's shared scheduler state");
    };
    let Some(sh) = (unsafe { Shared::attach(mem, len) }) else {
        fatal("shared scheduler state has the wrong layout");
    };
    let pid: u32 = std::env::var(shared::PROC_VAR)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| fatal("REWRITE_PROC missing"));
    sh.reclaim_after_exec();
    let mut s = sh.lock();
    if s.procs[pid as usize].killed {
        s.procs[pid as usize].signalled = true;
        drop(s);
        die();
    }
    let n = s.nthreads as usize;
    // After an `execve` the process has older, retired threads: take the
    // one that is still alive.
    let Some(me) = s.threads[..n]
        .iter()
        .position(|t| t.pid == pid && t.state != shared::T_EXITED)
    else {
        drop(s);
        fatal("launcher did not register this process");
    };
    s.threads[me].pthread = unsafe { libc::pthread_self() } as u64;
    drop(s);
    // Whoever started us waits for `P_LIVE`, so inherited sockets are
    // counted before it can close its own copies.
    crate::net::adopt_inherited(sh, pid);
    let mut s = sh.lock();
    if s.procs[pid as usize].killed {
        s.procs[pid as usize].signalled = true;
        drop(s);
        die();
    }
    let p = &mut s.procs[pid as usize];
    p.real_pid = unsafe { libc::getpid() };
    p.state = shared::P_LIVE;
    drop(s);
    PID.store(pid, Ordering::Relaxed);
    (sh, me)
}

/// No launcher: a run of this one process, with the baton already ours.
fn start_private_run(cfg: &Config) -> (&'static Shared, usize) {
    let mem = map_region(-1, Shared::SIZE);
    let sh = unsafe { Shared::init(mem, cfg.seed, cfg.quantum_lo, cfg.quantum_hi) };
    let mut s = sh.lock();
    let pid = s.add_proc(0, shared::NO_PROC);
    s.procs[pid as usize].state = shared::P_LIVE;
    s.procs[pid as usize].real_pid = unsafe { libc::getpid() };
    let me = s.add_thread(pid);
    s.threads[me].pthread = unsafe { libc::pthread_self() } as u64;
    s.hand_off(None, 0);
    drop(s);
    (sh, me)
}

/// Environment variable for runs without scheduling: the stubs get their
/// region but no scheduler, and no thread is registered, so every
/// interposer passes through. Measures the cost of the stubs alone.
const PASSIVE_VAR: &str = "REWRITE_PASSIVE";

/// Join the run's scheduler (or start a private one when there is no
/// launcher), then park the main thread until it is handed the baton.
pub fn init(info: Option<Info>, cfg: &Config) {
    *INFO.lock() = info;
    if let Some(spins) = std::env::var("REWRITE_PARK_SPINS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        shared::PARK_SPINS.store(spins, Ordering::Relaxed);
    }
    if std::env::var_os(PASSIVE_VAR).is_some() {
        map_region(-1, Shared::SIZE);
        return;
    }
    let mut key: libc::pthread_key_t = 0;
    let rc =
        unsafe { libc::pthread_key_create(&raw mut key, Some(crate::interpose::thread_teardown)) };
    assert_eq!(rc, 0, "pthread_key_create failed");
    ID_KEY.store(key as usize, Ordering::Relaxed);

    let (sh, me) = match std::env::var(shared::SHARED_VAR) {
        Ok(path) => {
            crate::coord::connect();
            crate::process::init();
            crate::io::init();
            crate::hostfs::init();
            join_run(&path)
        }
        Err(_) => start_private_run(cfg),
    };
    unsafe { scheduler_slot().write(rewrite_scheduler_yield as *const () as usize) };
    SHARED.store(std::ptr::from_ref(sh).cast_mut(), Ordering::Relaxed);
    open_trace();
    load_mask();
    // Its own stream, per process; a `fork` child carries its parent's on
    crate::alloc::seed_layout(process_seed(cfg.seed, pid()), cfg.heap_size);
    set_my_id(me);
    wait_for_baton(me);
    // With the baton: only its holder talks to the launcher
    crate::coord::announce_image();
}

/// In the child of a `fork`: become process `child`, whose main thread the
/// parent registered, and park until that thread is given the baton.
pub fn become_forked_child(child: u32) {
    let Some(sh) = shared() else { return };
    let me = {
        let mut s = sh.lock();
        let n = s.nthreads as usize;
        let me = s.threads[..n]
            .iter()
            .position(|t| t.pid == child)
            .expect("forked child has no thread record");
        s.threads[me].pthread = unsafe { libc::pthread_self() } as u64;
        me
    };
    crate::net::adopt_inherited(sh, child);
    {
        let mut s = sh.lock();
        let p = &mut s.procs[child as usize];
        p.real_pid = unsafe { libc::getpid() };
        p.state = shared::P_LIVE;
    }
    PID.store(child, Ordering::Relaxed);
    // Only the forking thread exists here, under a new port name. A thread
    // of the parent may have been inside `set_my_id` at the fork.
    PORTS.force_unlock();
    PORTS.lock().clear();
    crate::alloc::forked();
    crate::hostfs::forked();
    crate::signals::forked();
    crate::io::forked();
    crate::process::forked();
    crate::kq::forked();
    crate::determinism::forked(process_seed(Config::from_env().seed, child));
    HOOKS.store(0, Ordering::Relaxed);
    crate::io::IO_WAITS.store(0, Ordering::Relaxed);
    for c in &crate::interpose::COUNTS {
        c.store(0, Ordering::Relaxed);
    }
    set_my_id(me);
    wait_for_baton(me);
}

/// Register a thread created by the baton holder; it starts parked.
pub fn add_thread() -> usize {
    with(shared::State::add_thread).expect("scheduler not initialized")
}

/// Give up the baton with `state` recorded for the calling thread, and
/// block until it comes back. Must be called with the baton held and
/// without the scheduler lock.
/// `site` names the switch point for the schedule trace: the stub address
/// on quantum expiry, the blocked-on address for blocking calls.
pub fn yield_baton(state: State, site: u64) {
    let Some(me) = my_id() else { return };
    yield_baton_as(me, state, site, None);
}

/// Block on `key` until woken or until the virtual clock reaches
/// `deadline`. Returns true when the deadline ended the wait.
pub fn block_until(key: u64, deadline: Option<u64>) -> bool {
    let Some(me) = my_id() else { return false };
    yield_baton_as(me, State::Blocked(key as usize), key, deadline);
    with(|s, _| std::mem::take(&mut s.threads[me].timed_out)) == Some(true)
}

pub fn baton_is_mine() -> bool {
    let Some(me) = my_id() else { return false };
    with(|s, _| s.current as usize == me) == Some(true)
}

/// The virtual clock without advancing it, for computing deadlines
pub fn now() -> u64 {
    with(|s, _| s.clock_ns).unwrap_or(0)
}

/// The virtual clock as it stands, or None outside a run.
pub fn peek_clock() -> Option<u64> {
    with(|s, _| s.clock_ns)
}

/// A read of the virtual clock by the guest, or None outside a run.
pub fn clock_read() -> Option<u64> {
    with(|s, _| s.clock_read())
}

static YIELDS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// `yield_baton` for a caller that already knows its id (the teardown hook
/// runs after libpthread has cleared the key value).
pub fn yield_baton_as(me: usize, state: State, site: u64, deadline: Option<u64>) {
    let Some(sh) = shared() else { return };
    settle_hooks();
    // A thread that never parks still has to notice a dead launcher
    if YIELDS.fetch_add(1, Ordering::Relaxed).is_multiple_of(4096) {
        sh.exit_if_orphaned();
    }
    let (st, key) = match state {
        State::Runnable => (shared::T_RUNNABLE, 0),
        State::Blocked(k) => (shared::T_BLOCKED, k as u64),
        State::Exited => (shared::T_EXITED, 0),
    };
    let Some(mut s) = sh.lock_unless_reentrant() else {
        // A signal handler on the thread that is inside the lock
        return;
    };
    if s.current as usize != me {
        // Guest code on a thread that does not hold the baton: a signal
        // handler on a parked thread. It must not hand over what it lacks.
        drop(s);
        crate::report::log("a thread without the baton tried to yield (signal handler?); ignored");
        return;
    }
    s.threads[me].timed_out = false;
    if st == shared::T_BLOCKED {
        // An outside thread's wake is not serialized by the baton and may
        // have come since the caller decided to block: let it look again.
        let wakes = s.procs[pid() as usize].outside_wakes;
        if std::mem::replace(&mut s.threads[me].wakes_seen, wakes) != wakes {
            return;
        }
    }
    // A deadline of 0 would mean none; one in the past expires at once
    s.threads[me].deadline = deadline.map_or(0, |d| d.max(1));
    let handoff = s.hand_off(Some((me, st, key)), site);
    let after = After::of(&mut s, handoff);
    let stuck = state != State::Exited || s.any_alive();
    drop(s);
    if after.carry_out(sh, me, state, site) || !stuck {
        return;
    }

    // Nothing can run. A GCD worker may be about to wake one of us (a block
    // finishing under dispatch_sync, a semaphore): look again in real time.
    // The virtual clock and the trace do not move meanwhile.
    let outside = has_outside_threads();
    let anywhere = with(|s, pid| {
        s.procs[pid as usize].has_outside_threads |= outside;
        s.any_outside_threads()
    }) == Some(true);
    if !anywhere {
        describe_blocked();
        fatal("deadlock: every thread is blocked");
    }
    let began = std::time::Instant::now();
    loop {
        unsafe { libc::usleep(200) };
        sh.exit_if_orphaned();
        let mut s = sh.lock();
        let handoff = s.choose(Some(me), site);
        let after = After::of(&mut s, handoff);
        drop(s);
        if after.carry_out(sh, me, state, site) {
            return;
        }
        if began.elapsed() > std::time::Duration::from_secs(30) {
            fatal("deadlock: every thread is blocked, and no outside thread woke one");
        }
    }
}

/// What each blocked thread of the run waits for, for the deadlock report.
fn describe_blocked() {
    let mut text = String::new();
    with(|s, _| {
        let _ = writeln!(text, "blocked threads at virtual time {} ns:", s.clock_ns);
        for (id, t) in s.threads[..s.nthreads as usize].iter().enumerate() {
            if t.state != shared::T_BLOCKED {
                continue;
            }
            let what = match t.key {
                shared::WAIT_KEY => "a child to exit".to_string(),
                shared::IO_KEY => "I/O (read, accept, poll, kevent…)".to_string(),
                shared::SLEEP_KEY => "a sleep".to_string(),
                shared::RESTART_KEY => "its restart delay".to_string(),
                key => format!("a lock, condition or join at {key:#x}"),
            };
            let until = if t.deadline == 0 {
                String::new()
            } else {
                format!(" until {} ns", t.deadline)
            };
            let _ = writeln!(text, "  p{} t{id}: {what}{until}", t.pid);
        }
    });
    crate::report::log(text.trim_end());
}

/// What a hand-off decided, copied out from under the lock.
struct After {
    handoff: Handoff,
    quantum: i64,
    issued: u64,
    clock: u64,
    /// The hand-off crashed this process
    crashed_self: bool,
    /// Something is due when the baton is next taken up: a crashed process
    /// to wait out, a `SIGCHLD` to deliver
    unsettled: bool,
}

impl After {
    fn of(s: &mut shared::State, handoff: Handoff) -> After {
        After {
            handoff,
            quantum: s.pending_quantum,
            issued: s.issued,
            clock: s.clock_ns,
            crashed_self: s.procs[pid() as usize].killed,
            unsettled: signal_crashed(s) || s.procs[pid() as usize].child_deaths > 0,
        }
    }

    /// Returns false when the run was idle and the caller has to wait.
    fn carry_out(self, sh: &Shared, me: usize, state: State, site: u64) -> bool {
        match self.handoff {
            Handoff::Idle if self.crashed_self => die(),
            Handoff::Idle => false,
            Handoff::Stay => {
                // Traced too: a quantum ended here, switch or not, and
                // masking this site would move the run
                trace_switch(me, me, self.issued, site, self.clock);
                if self.unsettled {
                    take_up_baton(sh);
                } else {
                    install_quantum(self.quantum);
                }
                true
            }
            Handoff::Switch { to, seen } => {
                trace_switch(me, to, self.issued, site, self.clock);
                sh.unpark(to);
                // Our own crash comes after the baton is safely elsewhere
                if self.crashed_self {
                    die();
                }
                if state != State::Exited {
                    sh.park(me, seen);
                    take_up_baton(sh);
                }
                true
            }
        }
    }
}

/// Send `SIGKILL` to the processes the scheduler has crashed, other than
/// this one: its own crash waits until the baton is elsewhere. Returns
/// whether a crashed process may still be alive.
pub fn signal_crashed(s: &mut shared::State) -> bool {
    let own = unsafe { libc::getpid() };
    s.take_kills(|victim| {
        if victim != own {
            unsafe { libc::kill(victim, libc::SIGKILL) };
        }
    });
    s.unsettled > 0
}

/// The baton is ours: before running, wait in real time until every crashed
/// process is really dead, so that what its death releases in the kernel is
/// released at this point of the schedule and not at some later one.
pub fn take_up_baton(sh: &Shared) {
    wait_out_deaths(sh, true);
}

/// `take_up_baton` for a thread that already runs: its quantum stands.
pub fn settle_deaths() {
    if let Some(sh) = shared() {
        wait_out_deaths(sh, false);
    }
}

fn wait_out_deaths(sh: &Shared, new_quantum: bool) {
    // Counted, not timed: this process's clock is the virtual one
    const GIVE_UP_AFTER: u32 = 200_000;
    for polls in 0.. {
        let mut s = sh.lock();
        if !s.settle_deaths() && polls > GIVE_UP_AFTER {
            crate::report::log("a crashed process will not die; no longer waiting for it");
            s.forget_unsettled();
        }
        if s.settle_deaths() {
            if new_quantum {
                install_quantum(s.pending_quantum);
            }
            let me = &mut s.procs[pid() as usize];
            let mut about = None;
            if me.child_deaths > 0 && crate::signals::takes_sigchld() {
                me.child_deaths = 0;
                let child = me.last_dead_child;
                let status = s.procs[child as usize].exit_status;
                about = Some((shared::vpid_of(child), status));
            }
            drop(s);
            if let Some((child, status)) = about {
                crate::signals::deliver_sigchld(child, status);
            }
            return;
        }
        drop(s);
        // Only the launcher can tell us that the victim is dead
        if polls.is_multiple_of(1024) {
            sh.exit_if_orphaned();
        }
        unsafe { libc::usleep(50) };
    }
}

/// The scheduler crashed this process: its threads are already out of the
/// schedule, so all that is left is to go the way a crash goes.
fn die() -> ! {
    unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
    loop {
        unsafe { libc::pause() };
    }
}

/// Park a freshly registered thread until it is handed the baton for the
/// first time. Its park word is still zero: slots are never reused.
pub fn wait_for_baton(id: usize) {
    let Some(sh) = shared() else { return };
    sh.park(id, 0);
    take_up_baton(sh);
}

/// `REWRITE_TRACE=file`: one line per switch, appended by whoever gives the
/// baton away, so two runs can be compared switch by switch. The hash in
/// the report covers the same four values.
static TRACE_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// A debugging path: the launcher's, from the shared state, in a run that
/// has one; this process's own environment otherwise.
fn debug_path(
    name: &str,
    field: fn(&shared::State) -> &[u8; shared::DEBUG_PATH_LEN],
) -> Option<String> {
    if crate::coord::connected() {
        with(|s, _| shared::debug_path(field(s))).flatten()
    } else {
        std::env::var(name).ok()
    }
}

fn open_trace() {
    let Some(path) = debug_path("REWRITE_TRACE", |s| &s.trace_path) else {
        return;
    };
    let Ok(cpath) = std::ffi::CString::new(path) else {
        return;
    };
    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
            0o644,
        )
    };
    // Out of the way of the guest's descriptor numbers
    let high = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 200) };
    unsafe { libc::close(fd) };
    TRACE_FD.store(high, Ordering::Relaxed);
}

fn trace_switch(from: usize, to: usize, issued: u64, site: u64, clock: u64) {
    let fd = TRACE_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    let mut line = String::new();
    let _ = writeln!(
        line,
        "p{} t{from} -> t{to} issued={issued} site={site:#x} clock={clock}",
        pid()
    );
    unsafe { libc::write(fd, line.as_ptr().cast(), line.len()) };
}

/// Switch sites of interposed calls in the schedule trace: far above any
/// stub address, tagged with the kind of call.
pub const SITE_IO: u64 = 0xF100_0000_0000_0000;
pub const SITE_NET: u64 = 0xF200_0000_0000_0000;
pub const SITE_WAIT: u64 = 0xF300_0000_0000_0000;
pub const SITE_FILE: u64 = 0xF400_0000_0000_0000;

/// An interposed I/O call counts as one hook event, like a stub: the
/// quantum can expire at an I/O boundary. That puts switch points inside
/// a read-modify-write on a file even when only branches are hooked.
pub fn hook_event(site: u64) {
    if my_id().is_none() {
        return;
    }
    let Some(sh) = shared() else { return };
    let expired = unsafe {
        let counter = sh.counter();
        *counter -= 1;
        *counter == 0
    };
    if expired {
        rewrite_yield_impl(site);
    }
}

/// Sorted return addresses, as loaded, of this program's stubs at which a
/// quantum may not end (`REWRITE_MASK`: `<program> <address in the file>`).
/// Every stub still counts, so a run that never expires at a masked site is
/// the unmasked run. Set before any hook can expire; read without a lock,
/// since an expiry may come inside a signal handler.
static MASK: std::sync::OnceLock<Box<[u64]>> = std::sync::OnceLock::new();
/// Expiries deferred in a row. A loop of nothing but masked stubs would
/// otherwise keep the baton for ever.
static DEFERRED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
const MAX_DEFERRED: u32 = 100_000;
static SAID_MASKED_LOOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" {
    fn _dyld_get_image_vmaddr_slide(index: u32) -> isize;
    fn _dyld_image_count() -> u32;
    fn _dyld_get_image_header(index: u32) -> *const u32;
    fn _NSGetExecutablePath(buf: *mut libc::c_char, size: *mut u32) -> libc::c_int;
}

/// Word of a Mach-O header that holds the file type, and an executable's
const FILETYPE: usize = 3;
const MH_EXECUTE: u32 = 2;

fn load_mask() {
    let Some(text) = debug_path("REWRITE_MASK", |s| &s.mask_path)
        .and_then(|path| std::fs::read_to_string(path).ok())
    else {
        return;
    };
    let mut buf = [0 as libc::c_char; 4096];
    let mut size = buf.len() as u32;
    if unsafe { _NSGetExecutablePath(buf.as_mut_ptr(), &raw mut size) } != 0 {
        return;
    }
    let exe = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
    let file = exe.rsplit('/').next().unwrap_or_default();
    let program = file.rfind(shared::CACHE_TAG).map_or(file, |at| &file[..at]);
    // The executable is not image 0 when a library was inserted ahead of it
    let slide = (0..unsafe { _dyld_image_count() })
        .find(|&i| unsafe {
            let header = _dyld_get_image_header(i);
            !header.is_null() && *header.add(FILETYPE) == MH_EXECUTE
        })
        .map_or(0, |i| unsafe { _dyld_get_image_vmaddr_slide(i) }) as u64;
    // A program's name may hold spaces; an address cannot
    let mut pcs: Vec<u64> = text
        .lines()
        .filter_map(|l| l.rsplit_once(' '))
        .filter(|(name, _)| *name == program)
        .filter_map(|(_, pc)| u64::from_str_radix(pc, 16).ok())
        .map(|pc| pc.wrapping_add(slide))
        .collect();
    pcs.sort_unstable();
    let _ = MASK.set(pcs.into_boxed_slice());
}

fn masked(stub_pc: u64) -> bool {
    if MASK
        .get()
        .is_none_or(|m| m.binary_search(&stub_pc).is_err())
    {
        DEFERRED.store(0, Ordering::Relaxed);
        return false;
    }
    if DEFERRED.fetch_add(1, Ordering::Relaxed) < MAX_DEFERRED {
        return true;
    }
    if !SAID_MASKED_LOOP.swap(true, Ordering::Relaxed) {
        crate::report::log("a loop of masked sites never reaches another hook; switching at one");
    }
    DEFERRED.store(0, Ordering::Relaxed);
    false
}

/// Called from the stub's expired path through the register-saving
/// trampoline, with the guest's registers already preserved.
#[no_mangle]
pub extern "C" fn rewrite_yield_impl(stub_pc: u64) {
    if masked(stub_pc) {
        // Not here: one more event, so the switch falls on the next hook
        settle_hooks();
        install_quantum(1);
        return;
    }
    with(|s, _| s.expiries += 1);
    if my_id().is_some() {
        yield_baton(State::Runnable, stub_pc);
    } else if let Some(q) = with(|s, _| s.renew_quantum()) {
        settle_hooks();
        install_quantum(q);
    }
}

pub fn report(out: &mut String) {
    let _ = writeln!(out, "supervisor=loaded");
    if shared().is_none() {
        return;
    }
    settle_hooks();
    if let Some(info) = *INFO.lock() {
        let _ = writeln!(out, "seed={}", info.seed);
        let _ = writeln!(out, "sites={}", info.sites);
        let _ = writeln!(out, "mem_sites={}", info.mem_sites);
        let _ = writeln!(out, "hooks={}", HOOKS.load(Ordering::Relaxed));
    }
    let (threads, expiries, switches, hash) =
        with(|s, pid| (s.threads_of(pid), s.expiries, s.switches, s.trace_hash)).unwrap();
    let _ = writeln!(out, "proc={}", pid());
    let _ = writeln!(out, "threads={threads}");
    let _ = writeln!(out, "expiries={expiries}");
    let _ = writeln!(out, "switches={switches}");
    let _ = writeln!(out, "schedule_hash={hash:016x}");
    let _ = writeln!(out, "paths_refused={}", crate::hostfs::refused());
    let _ = writeln!(
        out,
        "outside_wakes={}",
        OUTSIDE_WAKES.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "io_waits={}",
        crate::io::IO_WAITS.load(Ordering::Relaxed)
    );
    for (name, c) in crate::interpose::COUNT_NAMES
        .iter()
        .zip(crate::interpose::COUNTS.iter())
    {
        let _ = writeln!(out, "interposed_{name}={}", c.load(Ordering::Relaxed));
    }
}

// Saves every register the guest may have live (the stub already saved x0,
// x1 and x30), calls the Rust scheduler, and restores them. x18 is the
// platform register and x19-x28 are callee-saved, so neither needs saving.
// The site is the return address the shared stub body saved last, at the
// top of the stack on entry: our own frame then puts it at [x29, #16].
std::arch::global_asm!(
    ".globl _rewrite_scheduler_yield",
    ".p2align 2",
    "_rewrite_scheduler_yield:",
    "stp x29, x30, [sp, #-16]!",
    "mov x29, sp",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "stp x8, x9, [sp, #-16]!",
    "stp x10, x11, [sp, #-16]!",
    "stp x12, x13, [sp, #-16]!",
    "stp x14, x15, [sp, #-16]!",
    "stp x16, x17, [sp, #-16]!",
    "mrs x2, nzcv",
    "mrs x3, fpsr",
    "stp x2, x3, [sp, #-16]!",
    "stp q0, q1, [sp, #-32]!",
    "stp q2, q3, [sp, #-32]!",
    "stp q4, q5, [sp, #-32]!",
    "stp q6, q7, [sp, #-32]!",
    "stp q8, q9, [sp, #-32]!",
    "stp q10, q11, [sp, #-32]!",
    "stp q12, q13, [sp, #-32]!",
    "stp q14, q15, [sp, #-32]!",
    "stp q16, q17, [sp, #-32]!",
    "stp q18, q19, [sp, #-32]!",
    "stp q20, q21, [sp, #-32]!",
    "stp q22, q23, [sp, #-32]!",
    "stp q24, q25, [sp, #-32]!",
    "stp q26, q27, [sp, #-32]!",
    "stp q28, q29, [sp, #-32]!",
    "stp q30, q31, [sp, #-32]!",
    "ldr x0, [x29, #16]",
    "bl _rewrite_yield_impl",
    "ldp q30, q31, [sp], #32",
    "ldp q28, q29, [sp], #32",
    "ldp q26, q27, [sp], #32",
    "ldp q24, q25, [sp], #32",
    "ldp q22, q23, [sp], #32",
    "ldp q20, q21, [sp], #32",
    "ldp q18, q19, [sp], #32",
    "ldp q16, q17, [sp], #32",
    "ldp q14, q15, [sp], #32",
    "ldp q12, q13, [sp], #32",
    "ldp q10, q11, [sp], #32",
    "ldp q8, q9, [sp], #32",
    "ldp q6, q7, [sp], #32",
    "ldp q4, q5, [sp], #32",
    "ldp q2, q3, [sp], #32",
    "ldp q0, q1, [sp], #32",
    "ldp x2, x3, [sp], #16",
    "msr nzcv, x2",
    "msr fpsr, x3",
    "ldp x16, x17, [sp], #16",
    "ldp x14, x15, [sp], #16",
    "ldp x12, x13, [sp], #16",
    "ldp x10, x11, [sp], #16",
    "ldp x8, x9, [sp], #16",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "ret",
);
