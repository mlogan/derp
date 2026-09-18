//! Baton scheduler. Every guest thread in every process of the run is a
//! real pthread parked on its word in the shared state; exactly one holds
//! the baton and runs guest code. Switches happen only at quantum expiry
//! (from the stubs) and at the interposed blocking primitives, so the
//! interleaving is a function of the seed.

use std::ffi::c_void;
use std::fmt::Write;
use std::sync::atomic::{AtomicI64, AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use crate::shared::{self, Handoff, Shared};
use crate::stubdata::Page;

pub struct Config {
    pub seed: u64,
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
        Config {
            seed,
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
static PAGE: AtomicUsize = AtomicUsize::new(0);
static MAPPED_FIXED: AtomicU32 = AtomicU32::new(0);
/// The quantum counter is per process until it moves into the shared
/// state, so hooks are accounted locally: `LAST_QUANTUM` is what was last
/// installed and `HOOKS` what earlier quanta consumed.
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
    (v != 0).then(|| v - 1)
}

pub fn set_my_id(id: usize) {
    let key = ID_KEY.load(Ordering::Relaxed) as libc::pthread_key_t;
    unsafe { libc::pthread_setspecific(key, (id + 1) as *const c_void) };
}

pub fn pid() -> u32 {
    PID.load(Ordering::Relaxed)
}

fn shared() -> Option<&'static Shared> {
    unsafe { SHARED.load(Ordering::Relaxed).as_ref() }
}

/// Run `f` on the locked scheduler state. `f` must not block or allocate.
pub fn with<R>(f: impl FnOnce(&mut shared::State, u32) -> R) -> Option<R> {
    let sh = shared()?;
    let mut s = sh.lock();
    Some(f(&mut s, pid()))
}

/// Make every thread of this process blocked on `addr` runnable.
pub fn wake_all(addr: usize) {
    with(|s, pid| s.wake_all(pid, addr as u64));
}

extern "C" {
    fn rewrite_scheduler_yield();
}

fn page() -> Option<Page> {
    Page::from_base(PAGE.load(Ordering::Relaxed))
}

/// Fold the hooks consumed from the current quantum into `HOOKS`.
fn settle_hooks() {
    if let Some(page) = page() {
        let remaining = unsafe { page.counter().read() };
        let last = LAST_QUANTUM.swap(remaining, Ordering::Relaxed);
        HOOKS.fetch_add(last - remaining, Ordering::Relaxed);
    }
}

fn install_quantum(q: i64) {
    if let Some(page) = page() {
        unsafe { page.counter().write(q) };
        LAST_QUANTUM.store(q, Ordering::Relaxed);
    }
}

extern "C" {
    static mach_task_self_: u32;
    fn mach_vm_allocate(task: u32, addr: *mut u64, size: u64, flags: i32) -> i32;
}

/// Map `len` bytes of `fd` (or anonymous memory for -1) at `MAP_ADDR`.
/// An `mmap` hint is not enough: the kernel ignores it now and then. A
/// fixed `mach_vm_allocate` fails instead of replacing what is there, so
/// mapping over that reservation with `MAP_FIXED` cannot clobber anything.
fn map_state(fd: i32, len: usize) -> Option<*mut Shared> {
    let mut addr = shared::MAP_ADDR as u64;
    let reserved = unsafe { mach_vm_allocate(mach_task_self_, &raw mut addr, len as u64, 0) } == 0;
    let mut flags = libc::MAP_SHARED;
    if reserved {
        flags |= libc::MAP_FIXED;
    }
    if fd < 0 {
        flags |= libc::MAP_ANON;
    }
    let p = unsafe {
        libc::mmap(
            shared::MAP_ADDR as *mut c_void,
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            flags,
            fd,
            0,
        )
    };
    (p != libc::MAP_FAILED).then_some(p.cast())
}

fn map_shared_file(path: &str) -> Option<(*mut Shared, usize)> {
    let cpath = std::ffi::CString::new(path).ok()?;
    unsafe {
        let fd = libc::open(cpath.as_ptr(), libc::O_RDWR);
        if fd < 0 {
            return None;
        }
        let mut st: libc::stat = std::mem::zeroed();
        let mapped = if libc::fstat(fd, &raw mut st) == 0 {
            let len = st.st_size as usize;
            map_state(fd, len).map(|p| (p, len))
        } else {
            None
        };
        libc::close(fd);
        mapped
    }
}

fn map_private_state() -> *mut Shared {
    map_state(-1, Shared::SIZE).unwrap_or_else(|| fatal("mapping scheduler state failed"))
}

fn fatal(msg: &str) -> ! {
    crate::report::log(msg);
    std::process::abort();
}

fn note_placement(mem: *mut Shared) {
    MAPPED_FIXED.store(
        u32::from(mem as usize == shared::MAP_ADDR),
        Ordering::Relaxed,
    );
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
    let mut s = sh.lock();
    let n = s.nthreads as usize;
    let Some(me) = s.threads[..n].iter().position(|t| t.pid == pid) else {
        drop(s);
        fatal("launcher did not register this process");
    };
    s.threads[me].pthread = unsafe { libc::pthread_self() } as u64;
    let p = &mut s.procs[pid as usize];
    p.real_pid = unsafe { libc::getpid() };
    p.mapped_at = mem as u64;
    p.state = shared::P_LIVE;
    drop(s);
    PID.store(pid, Ordering::Relaxed);
    note_placement(mem);
    (sh, me)
}

/// No launcher: a run of this one process, with the baton already ours.
fn start_private_run(cfg: &Config) -> (&'static Shared, usize) {
    let mem = map_private_state();
    let sh = unsafe { Shared::init(mem, cfg.seed, cfg.quantum_lo, cfg.quantum_hi) };
    let mut s = sh.lock();
    let pid = s.add_proc(0, 0);
    s.procs[pid as usize].state = shared::P_LIVE;
    s.procs[pid as usize].real_pid = unsafe { libc::getpid() };
    let me = s.add_thread(pid);
    s.threads[me].pthread = unsafe { libc::pthread_self() } as u64;
    s.hand_off(None, 0);
    drop(s);
    note_placement(mem);
    (sh, me)
}

/// Join the run's scheduler (or start a private one when there is no
/// launcher), then park the main thread until it is handed the baton.
pub fn init(page: Option<Page>, cfg: &Config) {
    let mut key: libc::pthread_key_t = 0;
    let rc =
        unsafe { libc::pthread_key_create(&raw mut key, Some(crate::interpose::thread_teardown)) };
    assert_eq!(rc, 0, "pthread_key_create failed");
    ID_KEY.store(key as usize, Ordering::Relaxed);
    if let Some(page) = page {
        PAGE.store(page.base(), Ordering::Relaxed);
        unsafe {
            page.slot()
                .write(rewrite_scheduler_yield as *const () as usize);
        }
    }

    let (sh, me) = match std::env::var(shared::SHARED_VAR) {
        Ok(path) => join_run(&path),
        Err(_) => start_private_run(cfg),
    };
    SHARED.store(std::ptr::from_ref(sh).cast_mut(), Ordering::Relaxed);
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
    yield_baton_as(me, state, site);
}

/// `yield_baton` for a caller that already knows its id (the teardown hook
/// runs after libpthread has cleared the key value).
pub fn yield_baton_as(me: usize, state: State, site: u64) {
    let Some(sh) = shared() else { return };
    settle_hooks();
    let (st, key) = match state {
        State::Runnable => (shared::T_RUNNABLE, 0),
        State::Blocked(k) => (shared::T_BLOCKED, k as u64),
        State::Exited => (shared::T_EXITED, 0),
    };
    let mut s = sh.lock();
    debug_assert_eq!(s.current as usize, me);
    match s.hand_off(Some((me, st, key)), site) {
        Handoff::Idle => {
            let stuck = state != State::Exited || s.any_alive();
            drop(s);
            if stuck {
                fatal("deadlock: every thread is blocked");
            }
        }
        Handoff::Stay => {
            let q = s.pending_quantum;
            drop(s);
            install_quantum(q);
        }
        Handoff::Switch { to, seen } => {
            drop(s);
            crate::determinism::on_switch();
            sh.unpark(to);
            if state != State::Exited {
                sh.park(me, seen);
                install_quantum(sh.lock().pending_quantum);
            }
        }
    }
}

/// Park a freshly registered thread until it is handed the baton for the
/// first time. Its park word is still zero: slots are never reused.
pub fn wait_for_baton(id: usize) {
    let Some(sh) = shared() else { return };
    sh.park(id, 0);
    install_quantum(sh.lock().pending_quantum);
}

/// Called from the stub's expired path through the register-saving
/// trampoline, with the guest's registers already preserved.
#[no_mangle]
pub extern "C" fn rewrite_yield_impl(stub_pc: u64) {
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
    if let Some(page) = page() {
        let _ = writeln!(out, "seed={}", page.seed());
        let _ = writeln!(out, "sites={}", page.sites());
        let _ = writeln!(out, "mem_sites={}", page.mem_sites());
        let _ = writeln!(out, "hooks={}", HOOKS.load(Ordering::Relaxed));
    }
    let (threads, expiries, switches, hash) =
        with(|s, pid| (s.threads_of(pid), s.expiries, s.switches, s.trace_hash)).unwrap();
    let _ = writeln!(out, "proc={}", pid());
    let _ = writeln!(out, "threads={threads}");
    let _ = writeln!(out, "expiries={expiries}");
    let _ = writeln!(out, "switches={switches}");
    let _ = writeln!(out, "schedule_hash={hash:016x}");
    let _ = writeln!(
        out,
        "shared_fixed={}",
        MAPPED_FIXED.load(Ordering::Relaxed) == 1
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
    "mov x0, x30",
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
