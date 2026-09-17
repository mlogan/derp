//! Baton scheduler. Every guest thread is a real pthread parked on a mach
//! semaphore; exactly one holds the baton and runs guest code. Switches
//! happen only at quantum expiry (from the stubs) and at the interposed
//! blocking primitives, so the interleaving is a function of the seed.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::fmt::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::rng::Rng;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Runnable,
    Blocked(usize),
    Exited,
}

pub struct Thread {
    pub sem: u32,
    pub state: State,
    pub pthread: libc::pthread_t,
    /// Set by `cond_signal`; consumed by the waiter
    pub signaled: bool,
    /// Waiting with a timeout: may be released with ETIMEDOUT when nothing
    /// else can run
    pub timed: bool,
    pub timed_out: bool,
}

pub struct Sched {
    page: Option<Page>,
    rng: Rng,
    quantum_lo: u32,
    quantum_hi: u32,
    pub threads: Vec<Thread>,
    current: usize,
    issued: u64,
    switches: u64,
    expiries: u64,
    trace_hash: u64,
    /// FIFO of waiters per condition variable address
    pub cond_waiters: Vec<(usize, VecDeque<usize>)>,
}

pub static SCHED: Mutex<Option<Sched>> = Mutex::new(None);

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

extern "C" {
    static mach_task_self_: u32;
    fn semaphore_create(task: u32, sem: *mut u32, policy: i32, value: i32) -> i32;
    fn semaphore_wait(sem: u32) -> i32;
    fn semaphore_signal(sem: u32) -> i32;
    fn rewrite_scheduler_yield();
}

fn new_semaphore() -> u32 {
    let mut sem = 0;
    let rc = unsafe { semaphore_create(mach_task_self_, &raw mut sem, 0, 0) };
    assert_eq!(rc, 0, "semaphore_create failed");
    sem
}

fn park(sem: u32) {
    // KERN_ABORTED (14) after a signal: just wait again
    while unsafe { semaphore_wait(sem) } == 14 {}
}

fn fnv(mut h: u64, v: u64) -> u64 {
    for b in v.to_le_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01B3);
    }
    h
}

pub fn init(page: Option<Page>, cfg: &Config) {
    let mut key: libc::pthread_key_t = 0;
    let rc =
        unsafe { libc::pthread_key_create(&raw mut key, Some(crate::interpose::thread_teardown)) };
    assert_eq!(rc, 0, "pthread_key_create failed");
    ID_KEY.store(key as usize, Ordering::Relaxed);
    let mut rng = Rng::seed_from_u64(cfg.seed);
    let first = u64::from(rng.range_inclusive(cfg.quantum_lo, cfg.quantum_hi));
    if let Some(page) = page {
        unsafe {
            page.counter().write(first as i64);
            page.slot()
                .write(rewrite_scheduler_yield as *const () as usize);
        }
    }
    let main = Thread {
        sem: new_semaphore(),
        state: State::Running,
        pthread: unsafe { libc::pthread_self() },
        signaled: false,
        timed: false,
        timed_out: false,
    };
    *SCHED.lock().unwrap() = Some(Sched {
        page,
        rng,
        quantum_lo: cfg.quantum_lo,
        quantum_hi: cfg.quantum_hi,
        threads: vec![main],
        current: 0,
        issued: first,
        switches: 0,
        expiries: 0,
        trace_hash: 0xCBF2_9CE4_8422_2325,
        cond_waiters: Vec::new(),
    });
    set_my_id(0);
}

impl Sched {
    /// Register a thread created by the baton holder; it starts parked.
    pub fn add_thread(&mut self) -> usize {
        self.threads.push(Thread {
            sem: new_semaphore(),
            state: State::Runnable,
            pthread: 0,
            signaled: false,
            timed: false,
            timed_out: false,
        });
        self.threads.len() - 1
    }

    pub fn find_pthread(&self, t: libc::pthread_t) -> Option<usize> {
        self.threads.iter().position(|th| th.pthread == t)
    }

    /// Make every thread blocked on `addr` runnable.
    pub fn wake_all(&mut self, addr: usize) {
        for t in &mut self.threads {
            if t.state == State::Blocked(addr) {
                t.state = State::Runnable;
            }
        }
    }

    pub fn wake_thread(&mut self, id: usize) {
        if matches!(self.threads[id].state, State::Blocked(_)) {
            self.threads[id].state = State::Runnable;
        }
    }

    fn next_quantum(&mut self) -> i64 {
        let q = u64::from(self.rng.range_inclusive(self.quantum_lo, self.quantum_hi));
        self.issued += q;
        q as i64
    }

    fn reset_counter(&mut self) {
        let q = self.next_quantum();
        if let Some(page) = self.page {
            unsafe { page.counter().write(q) };
        }
    }

    /// Choose the next baton holder. Returns None when nothing can run.
    fn pick(&mut self) -> Option<usize> {
        let runnable: Vec<usize> = self
            .threads
            .iter()
            .enumerate()
            .filter(|(_, t)| t.state == State::Runnable)
            .map(|(i, _)| i)
            .collect();
        if runnable.is_empty() {
            // Idle: let a timed waiter time out, lowest id first
            let timed = self
                .threads
                .iter()
                .position(|t| t.timed && matches!(t.state, State::Blocked(_)))?;
            self.threads[timed].timed_out = true;
            self.threads[timed].state = State::Runnable;
            return Some(timed);
        }
        let i = self.rng.below(runnable.len() as u64) as usize;
        Some(runnable[i])
    }
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
    let mut guard = SCHED.lock().unwrap();
    let Some(s) = guard.as_mut() else { return };
    debug_assert_eq!(s.current, me);
    s.threads[me].state = state;
    let Some(next) = s.pick() else {
        if state == State::Exited {
            return;
        }
        drop(guard);
        crate::report::log("deadlock: every thread is blocked");
        std::process::abort();
    };
    s.reset_counter();
    if next == me {
        s.threads[me].state = State::Running;
        return;
    }
    s.switches += 1;
    crate::determinism::on_switch();
    s.trace_hash = fnv(
        fnv(fnv(fnv(s.trace_hash, me as u64), next as u64), s.issued),
        site,
    );
    s.threads[next].state = State::Running;
    s.current = next;
    let next_sem = s.threads[next].sem;
    let my_sem = s.threads[me].sem;
    drop(guard);
    unsafe { semaphore_signal(next_sem) };
    if state != State::Exited {
        park(my_sem);
    }
}

/// Park a freshly created thread until it is handed the baton.
pub fn wait_for_baton(id: usize) {
    let sem = SCHED.lock().unwrap().as_ref().unwrap().threads[id].sem;
    park(sem);
}

/// Called from the stub's expired path through the register-saving
/// trampoline, with the guest's registers already preserved.
#[no_mangle]
pub extern "C" fn rewrite_yield_impl(stub_pc: u64) {
    if let Some(s) = SCHED.lock().unwrap().as_mut() {
        s.expiries += 1;
    }
    if my_id().is_some() {
        yield_baton(State::Runnable, stub_pc);
    } else if let Some(s) = SCHED.lock().unwrap().as_mut() {
        s.reset_counter();
    }
}

pub fn report(out: &mut String) {
    let guard = SCHED.lock().unwrap();
    let _ = writeln!(out, "supervisor=loaded");
    let Some(st) = guard.as_ref() else { return };
    if let Some(page) = st.page {
        let remaining = unsafe { page.counter().read() };
        let _ = writeln!(out, "seed={}", page.seed());
        let _ = writeln!(out, "sites={}", page.sites());
        let _ = writeln!(out, "mem_sites={}", page.mem_sites());
        let _ = writeln!(out, "hooks={}", st.issued as i64 - remaining);
    }
    let _ = writeln!(out, "threads={}", st.threads.len());
    let _ = writeln!(out, "expiries={}", st.expiries);
    let _ = writeln!(out, "switches={}", st.switches);
    let _ = writeln!(out, "schedule_hash={:016x}", st.trace_hash);
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
