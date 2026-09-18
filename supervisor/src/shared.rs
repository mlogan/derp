//! Scheduler state shared by every process in a run. The launcher creates
//! a file holding one `Shared`, and every guest maps it `MAP_SHARED`.
//! Exactly one thread in the whole run holds the baton; everyone else is
//! parked on its own `park` word with a shared compare-and-wait ulock.
//!
//! The state holds indices only, never pointers, and nothing here may
//! allocate: the lock is a spinlock and holders must not sleep.
//!
//! Compiled into both the supervisor and the launcher.

use std::cell::UnsafeCell;
use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::rng::Rng;

pub const MAGIC: u64 = 0x0031_4448_5357_5252;
pub const MAX_THREADS: usize = 1024;
pub const MAX_PROCS: usize = 256;
/// Base of the fixed region every guest sets up. The stubs materialize
/// this address with one `movz`, so it has a single non-zero 16-bit chunk.
/// Low addresses are unreliable: whatever the kernel places first above the
/// dyld shared region varies between launches. Above the GPU carveout
/// (which ends at `0x70_0000_0000`) only hinted mappings appear, growing up
/// from its end.
pub const STUB_BASE: usize = 0x78_0000_0000;
/// The first page of the region is private to each process; the stubs
/// load the scheduler entry point from `STUB_BASE + SLOT_OFFSET`.
pub const PRIVATE_SIZE: usize = 0x4000;
pub const SLOT_OFFSET: u32 = 0;
/// Where guests map the file
pub const MAP_ADDR: usize = STUB_BASE + PRIVATE_SIZE;
/// Offset of the quantum counter from `STUB_BASE`, in reach of an
/// unsigned-offset `ldr`
pub const COUNTER_OFFSET: u32 = PRIVATE_SIZE as u32 + 16;
const _: () = assert!(COUNTER_OFFSET.is_multiple_of(8) && COUNTER_OFFSET < 0x8000);
const _: () = assert!(STUB_BASE & !(0xFFFF << 32) == 0);
/// Environment variable naming the shared file
pub const SHARED_VAR: &str = "REWRITE_SHARED";
/// Environment variable carrying the guest's process index
pub const PROC_VAR: &str = "REWRITE_PROC";
/// Switch site recorded when the launcher hands the baton on for a dead process
pub const SITE_PROCESS_DIED: u64 = u64::MAX;
/// Key a process's threads block on in `waitpid`; never a real address
pub const WAIT_KEY: u64 = 0x7FFF_FFFF_0000;
/// `ProcRec::parent` of the processes the launcher started
pub const NO_PROC: u32 = u32::MAX;
/// First virtual pid; process `i` of the run is `VPID_BASE + i`. The
/// launcher appears to guests as pid 1.
pub const VPID_BASE: i32 = 1000;

/// The run's one launcher socket, inherited by every guest at this
/// descriptor: high enough that guest code does not pick it for `dup2`,
/// below the default soft limit of 256.
pub const COORD_FD: i32 = 240;
/// Frame from a guest: `u32 length` of what follows, `u8 type`, `u32 proc`,
/// payload. Reply to `MSG_SPAWN` and `MSG_SPAWNED`: `i32 errno`, `u32
/// length`, payload. Only the baton holder talks, so frames never interleave.
pub const MSG_SPAWN: u8 = 1;
pub const MSG_SPAWNED: u8 = 2;
pub const MSG_REPORT: u8 = 3;

pub fn vpid_of(proc_index: u32) -> i32 {
    VPID_BASE + proc_index as i32
}

pub fn proc_of(vpid: i32) -> Option<u32> {
    (vpid >= VPID_BASE && vpid < VPID_BASE + MAX_PROCS as i32).then(|| (vpid - VPID_BASE) as u32)
}

pub const T_FREE: u32 = 0;
pub const T_RUNNABLE: u32 = 1;
pub const T_RUNNING: u32 = 2;
pub const T_BLOCKED: u32 = 3;
pub const T_EXITED: u32 = 4;

pub const P_FREE: u32 = 0;
/// Registered by its creator; the process has not attached yet
pub const P_STARTING: u32 = 1;
pub const P_LIVE: u32 = 2;
pub const P_EXITED: u32 = 3;

const NO_THREAD: u32 = u32::MAX;

#[repr(C)]
pub struct ThreadRec {
    pub state: u32,
    pub pid: u32,
    /// What a blocked thread waits for, namespaced by `pid`: addresses
    /// collide across guests because their layouts are identical.
    pub key: u64,
    /// `pthread_t` in the owning process, for `pthread_join`
    pub pthread: u64,
    /// Condition variable this thread is queued on (0: none) and its
    /// position in that queue; lowest `cond_seq` is signaled first.
    pub cond_key: u64,
    pub cond_seq: u64,
    pub signaled: bool,
    /// Waiting with a timeout: may be released with ETIMEDOUT when nothing
    /// else can run
    pub timed: bool,
    pub timed_out: bool,
    /// Bumped by whoever hands this thread the baton
    pub park: AtomicU32,
}

#[repr(C)]
pub struct ProcRec {
    pub state: u32,
    pub real_pid: i32,
    pub host: u32,
    pub parent: u32,
    /// Raw wait status, valid once `state` is `P_EXITED`
    pub exit_status: i32,
    /// The parent has collected the exit with `waitpid`
    pub reaped: bool,
    /// Address the process mapped the shared file at, for the launcher's
    /// placement check
    pub mapped_at: u64,
}

#[repr(C)]
pub struct State {
    pub rng: Rng,
    pub quantum_lo: u32,
    pub quantum_hi: u32,
    pub issued: u64,
    pub switches: u64,
    pub expiries: u64,
    pub trace_hash: u64,
    /// Quantum for whichever thread runs next, installed by the receiver
    /// so that hooks can be attributed to processes
    pub pending_quantum: i64,
    pub current: u32,
    pub nthreads: u32,
    pub nprocs: u32,
    next_cond_seq: u64,
    pub threads: [ThreadRec; MAX_THREADS],
    pub procs: [ProcRec; MAX_PROCS],
}

#[repr(C)]
pub struct Shared {
    magic: u64,
    size: u64,
    /// Hook events left in the quantum. The stubs decrement it without the
    /// lock: only the baton holder runs guest code.
    counter: UnsafeCell<i64>,
    lock: AtomicU32,
    state: UnsafeCell<State>,
}

unsafe impl Sync for Shared {}

pub struct Guard<'a> {
    shared: &'a Shared,
}

impl std::ops::Deref for Guard<'_> {
    type Target = State;
    fn deref(&self) -> &State {
        unsafe { &*self.shared.state.get() }
    }
}

impl std::ops::DerefMut for Guard<'_> {
    fn deref_mut(&mut self) -> &mut State {
        unsafe { &mut *self.shared.state.get() }
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.shared.lock.store(0, Ordering::Release);
    }
}

/// Outcome of giving up the baton
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handoff {
    /// The caller was chosen again
    Stay,
    /// `to` now holds the baton and must be woken; `seen` is the caller's
    /// park word at the time of the switch, to wait on
    Switch { to: usize, seen: u32 },
    /// Nothing can run
    Idle,
}

// The raw ulock calls rather than `os_sync_wait_on_address`: that wrapper
// lives in libSystem, so its ulock calls would land in the supervisor's
// own interposers. Calls made from the interposing image are not rebound.
extern "C" {
    fn __ulock_wait2(op: u32, addr: *mut c_void, value: u64, timeout_ns: u64, value2: u64)
        -> c_int;
    fn __ulock_wake(op: u32, addr: *mut c_void, wake_value: u64) -> c_int;
}

const UL_COMPARE_AND_WAIT_SHARED: u32 = 3;
const ULF_NO_ERRNO: u32 = 0x0100_0000;
const PARK_OP: u32 = UL_COMPARE_AND_WAIT_SHARED | ULF_NO_ERRNO;

pub fn fnv(mut h: u64, v: u64) -> u64 {
    for b in v.to_le_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01B3);
    }
    h
}

impl Shared {
    pub const SIZE: usize = std::mem::size_of::<Shared>();

    /// Initialize zero-filled memory of at least `SIZE` bytes.
    ///
    /// # Safety
    /// `mem` must point to `SIZE` writable, zeroed, page-aligned bytes that
    /// outlive the returned reference.
    pub unsafe fn init<'a>(
        mem: *mut Shared,
        seed: u64,
        quantum_lo: u32,
        quantum_hi: u32,
    ) -> &'a Shared {
        (&raw mut (*mem).magic).write(MAGIC);
        (&raw mut (*mem).size).write(Self::SIZE as u64);
        let shared = &*mem;
        let mut s = shared.lock();
        s.rng = Rng::seed_from_u64(seed);
        s.quantum_lo = quantum_lo;
        s.quantum_hi = quantum_hi;
        s.trace_hash = 0xCBF2_9CE4_8422_2325;
        s.current = NO_THREAD;
        s.next_cond_seq = 1;
        drop(s);
        shared
    }

    /// # Safety
    /// `mem` must point to a page-aligned mapping of `len` bytes of a file
    /// prepared by `init`.
    pub unsafe fn attach<'a>(mem: *mut Shared, len: usize) -> Option<&'a Shared> {
        if len < Self::SIZE {
            return None;
        }
        let shared = &*mem;
        (shared.magic == MAGIC && shared.size == Self::SIZE as u64).then_some(shared)
    }

    pub fn lock(&self) -> Guard<'_> {
        while self
            .lock
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::hint::spin_loop();
        }
        Guard { shared: self }
    }

    pub fn counter(&self) -> *mut i64 {
        self.counter.get()
    }

    fn park_word(&self, id: usize) -> *mut c_void {
        let state = self.state.get();
        unsafe { (&raw const (*state).threads[id].park).cast_mut().cast() }
    }

    /// Wake thread `id` after its park word was bumped under the lock.
    pub fn unpark(&self, id: usize) {
        // ENOENT when the thread has not reached its wait yet; it will see
        // the bumped word and not sleep.
        unsafe { __ulock_wake(PARK_OP, self.park_word(id), 0) };
    }

    /// Sleep until thread `id`'s park word differs from `seen`.
    pub fn park(&self, id: usize, seen: u32) {
        let word = self.park_word(id);
        let atomic = unsafe { &*word.cast::<AtomicU32>() };
        while atomic.load(Ordering::Acquire) == seen {
            unsafe { __ulock_wait2(PARK_OP, word, u64::from(seen), 0, 0) };
        }
    }
}

impl State {
    pub fn add_proc(&mut self, host: u32, parent: u32) -> u32 {
        let pid = self.nprocs as usize;
        assert!(pid < MAX_PROCS, "too many processes in one run");
        self.nprocs += 1;
        let p = &mut self.procs[pid];
        p.state = P_STARTING;
        p.host = host;
        p.parent = parent;
        pid as u32
    }

    /// Register a thread of `pid`; it starts parked and runnable. Slots are
    /// never reused, so ids are a function of creation order.
    pub fn add_thread(&mut self, pid: u32) -> usize {
        let id = self.nthreads as usize;
        assert!(id < MAX_THREADS, "too many threads in one run");
        self.nthreads += 1;
        let t = &mut self.threads[id];
        t.state = T_RUNNABLE;
        t.pid = pid;
        id
    }

    fn live(&mut self) -> &mut [ThreadRec] {
        let n = self.nthreads as usize;
        &mut self.threads[..n]
    }

    pub fn threads_of(&self, pid: u32) -> usize {
        self.threads[..self.nthreads as usize]
            .iter()
            .filter(|t| t.pid == pid)
            .count()
    }

    /// Latest thread of `pid` with this `pthread_t` (handles can be reused
    /// after a join).
    pub fn find_pthread(&self, pid: u32, pthread: u64) -> Option<usize> {
        self.threads[..self.nthreads as usize]
            .iter()
            .rposition(|t| t.pid == pid && t.pthread == pthread)
    }

    /// Make every thread of `pid` blocked on `key` runnable.
    pub fn wake_all(&mut self, pid: u32, key: u64) {
        for t in self.live() {
            if t.state == T_BLOCKED && t.pid == pid && t.key == key {
                t.state = T_RUNNABLE;
            }
        }
    }

    pub fn wake_thread(&mut self, id: usize) {
        if self.threads[id].state == T_BLOCKED {
            self.threads[id].state = T_RUNNABLE;
        }
    }

    pub fn cond_enqueue(&mut self, id: usize, cond: u64, timed: bool) {
        let seq = self.next_cond_seq;
        self.next_cond_seq += 1;
        let t = &mut self.threads[id];
        t.signaled = false;
        t.timed = timed;
        t.timed_out = false;
        t.cond_key = cond;
        t.cond_seq = seq;
    }

    /// Signal the longest-waiting thread of `pid` queued on `cond`, or all.
    pub fn cond_wake(&mut self, pid: u32, cond: u64, all: bool) {
        loop {
            let first = self
                .live()
                .iter()
                .enumerate()
                .filter(|(_, t)| t.pid == pid && t.cond_key == cond)
                .min_by_key(|(_, t)| t.cond_seq)
                .map(|(i, _)| i);
            let Some(id) = first else { return };
            self.threads[id].cond_key = 0;
            self.threads[id].signaled = true;
            self.wake_thread(id);
            if !all {
                return;
            }
        }
    }

    fn next_quantum(&mut self) -> i64 {
        let q = u64::from(self.rng.range_inclusive(self.quantum_lo, self.quantum_hi));
        self.issued += q;
        q as i64
    }

    /// Draw a fresh quantum for a thread that keeps running.
    pub fn renew_quantum(&mut self) -> i64 {
        self.pending_quantum = self.next_quantum();
        self.pending_quantum
    }

    /// Choose the next baton holder. Returns None when nothing can run.
    fn pick(&mut self) -> Option<usize> {
        let runnable = self.live().iter().filter(|t| t.state == T_RUNNABLE).count();
        if runnable == 0 {
            // Idle: let a timed waiter time out, lowest id first
            let timed = self
                .live()
                .iter()
                .position(|t| t.timed && t.state == T_BLOCKED)?;
            self.threads[timed].timed_out = true;
            self.threads[timed].state = T_RUNNABLE;
            return Some(timed);
        }
        let n = self.rng.below(runnable as u64) as usize;
        self.live()
            .iter()
            .enumerate()
            .filter(|(_, t)| t.state == T_RUNNABLE)
            .nth(n)
            .map(|(i, _)| i)
    }

    /// Record the caller's new state and pass the baton. `from` is None
    /// for the first handoff of a run. `site` names the switch point for
    /// the schedule trace. On `Switch`, the caller must `unpark(to)` after
    /// dropping the lock.
    pub fn hand_off(&mut self, from: Option<(usize, u32, u64)>, site: u64) -> Handoff {
        let me = from.map(|(id, state, key)| {
            self.threads[id].state = state;
            self.threads[id].key = key;
            id
        });
        let Some(next) = self.pick() else {
            return Handoff::Idle;
        };
        self.pending_quantum = self.next_quantum();
        self.threads[next].state = T_RUNNING;
        if me == Some(next) {
            return Handoff::Stay;
        }
        if let Some(me) = me {
            self.switches += 1;
            let h = fnv(fnv(self.trace_hash, me as u64), next as u64);
            self.trace_hash = fnv(fnv(h, self.issued), site);
        }
        self.current = next as u32;
        self.threads[next].park.fetch_add(1, Ordering::Release);
        let seen = me.map_or(0, |me| self.threads[me].park.load(Ordering::Relaxed));
        Handoff::Switch { to: next, seen }
    }

    /// A process is gone (exit, `_exit`, crash): retire its threads, and if
    /// one of them held the baton pass it on. Only the launcher calls this,
    /// after reaping, so nothing of the process can still be running.
    pub fn process_died(&mut self, pid: u32, status: i32) -> Handoff {
        self.procs[pid as usize].state = P_EXITED;
        self.procs[pid as usize].exit_status = status;
        let parent = self.procs[pid as usize].parent;
        if parent != NO_PROC {
            self.wake_all(parent, WAIT_KEY);
        }
        let mut held = None;
        for (i, t) in self.live().iter_mut().enumerate() {
            if t.pid == pid && t.state != T_EXITED {
                if t.state == T_RUNNING {
                    held = Some(i);
                }
                t.state = T_EXITED;
                t.cond_key = 0;
            }
        }
        match held {
            Some(id) => self.hand_off(Some((id, T_EXITED, 0)), SITE_PROCESS_DIED),
            None => Handoff::Stay,
        }
    }

    /// An exited, unreaped child of `parent`: the one named, or the lowest.
    pub fn exited_child(&self, parent: u32, which: Option<u32>) -> Option<u32> {
        (0..self.nprocs).find(|&i| {
            let p = &self.procs[i as usize];
            p.parent == parent && p.state == P_EXITED && !p.reaped && which.is_none_or(|w| w == i)
        })
    }

    /// Whether `parent` has a child it could still wait for.
    pub fn has_child(&self, parent: u32, which: Option<u32>) -> bool {
        (0..self.nprocs).any(|i| {
            let p = &self.procs[i as usize];
            p.parent == parent && !p.reaped && which.is_none_or(|w| w == i)
        })
    }

    /// True while some thread that has not exited belongs to a live process.
    pub fn any_alive(&self) -> bool {
        self.threads[..self.nthreads as usize]
            .iter()
            .any(|t| t.state != T_EXITED && t.state != T_FREE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stubs_can_reach_the_counter() {
        let offset = std::mem::offset_of!(Shared, counter);
        assert_eq!(PRIVATE_SIZE + offset, COUNTER_OFFSET as usize);
    }
}
