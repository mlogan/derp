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
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use crate::rng::Rng;

#[path = "netstate.rs"]
pub mod netstate;

pub const MAGIC: u64 = 0x0031_4448_5357_5252;
/// Slots are never reused, so these bound what a whole run may create. A
/// server with a thread per request gets through a thousand quickly.
pub const MAX_THREADS: usize = 16384;
pub const MAX_PROCS: usize = 1024;
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
/// Key of threads parked until a descriptor becomes ready. Readiness
/// crosses processes, so these are woken without regard to `pid`.
pub const IO_KEY: u64 = 0x7FFF_FFFF_0001;
/// Pipes and sockets the launcher itself was given (as `dev:ino` pairs):
/// their other end is outside the run, so guests block on them for real.
pub const EXTERNAL_VAR: &str = "REWRITE_EXTERNAL";
/// The directory of the guest's virtual host, which its path names are
/// held to, and extra locations every host may touch (`:`-separated)
pub const HOST_ROOT_VAR: &str = "REWRITE_HOST_ROOT";
pub const ALLOW_VAR: &str = "REWRITE_ALLOW";
/// Key of a restarted process's main thread until its restart delay is
/// over: nothing wakes it but the deadline
pub const RESTART_KEY: u64 = 0x7FFF_FFFF_0003;

pub const RESTART_NEVER: u8 = 0;
/// After a signal or a non-zero exit
pub const RESTART_ON_FAILURE: u8 = 1;
pub const RESTART_ALWAYS: u8 = 2;
pub const NO_LIMIT: u32 = u32::MAX;

/// What the run file says about a process's crashes and restarts. All
/// times are virtual. Zeroed means: never crashed on purpose, never
/// restarted.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Faults {
    pub restart: u8,
    /// How long the process stays down before its next life may run, drawn
    /// from this range at each death. While it is down its host has no such
    /// process: connections to it are refused.
    pub restart_delay_lo_ns: u64,
    pub restart_delay_hi_ns: u64,
    /// `NO_LIMIT`, or how many restarts are left
    pub restarts_left: u32,
    /// A crash is injected this far into each life, drawn from the range
    /// (both 0: none)
    pub crash_lo_ns: u64,
    pub crash_hi_ns: u64,
    /// `NO_LIMIT`, or how many crashes are left to inject
    pub crashes_left: u32,
}

pub const DEBUG_PATH_LEN: usize = 1024;

/// What the cache puts between a program's file name and the key of its
/// rewritten copy
pub const CACHE_TAG: &str = ".rw3-";

/// Store `path` in one of the state's path fields; too long is not stored.
pub fn set_debug_path(field: &mut [u8; DEBUG_PATH_LEN], path: &str) {
    if path.len() < DEBUG_PATH_LEN {
        field[..path.len()].copy_from_slice(path.as_bytes());
        field[path.len()] = 0;
    }
}

#[must_use]
pub fn debug_path(field: &[u8; DEBUG_PATH_LEN]) -> Option<String> {
    let len = field.iter().position(|&b| b == 0)?;
    (len > 0).then(|| String::from_utf8_lossy(&field[..len]).into_owned())
}

/// Raw wait status of a process that died of `SIGKILL`
const KILLED_STATUS: i32 = 9;

/// `ProcRec::parent` of the processes the launcher started
pub const NO_PROC: u32 = u32::MAX;
/// First virtual pid; process `i` of the run is `VPID_BASE + i`. Real pids
/// on macOS never exceed 99,999, so the two ranges cannot collide: a pid at
/// or above this is ours and anything below is the kernel's, whoever asks.
/// A virtual pid that leaks into an untranslated kernel call fails with
/// "no such process" instead of naming a stranger. The launcher appears to
/// guests as pid 1.
pub const VPID_BASE: i32 = 100_000;

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

/// The process a virtual pid names; None for a real pid.
pub fn proc_of(vpid: i32) -> Option<u32> {
    (vpid >= VPID_BASE && vpid < VPID_BASE + MAX_PROCS as i32).then(|| (vpid - VPID_BASE) as u32)
}

pub fn is_virtual_pid(pid: i32) -> bool {
    pid >= VPID_BASE
}

pub const PER_READ_NS: u64 = 1_000;
pub const PER_YIELD_NS: u64 = 1_000_000;
/// Key of sleeping threads: nothing wakes it but the deadline
pub const SLEEP_KEY: u64 = 0x7FFF_FFFF_0002;

pub const T_RUNNABLE: u32 = 1;
pub const T_RUNNING: u32 = 2;
pub const T_BLOCKED: u32 = 3;
pub const T_EXITED: u32 = 4;

/// Registered by its creator; the process has not attached yet
pub const P_STARTING: u32 = 1;
pub const P_LIVE: u32 = 2;
pub const P_EXITED: u32 = 3;

const NO_THREAD: u32 = u32::MAX;

/// Iterations a parking thread spins before it sleeps (`REWRITE_PARK_SPINS`)
pub static PARK_SPINS: AtomicU32 = AtomicU32::new(0);

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
    /// Set when the wait ended because `deadline` passed
    pub timed_out: bool,
    /// `ProcRec::outside_wakes` when this thread last looked
    pub wakes_seen: u64,
    /// Virtual time at which a blocked thread gives up waiting (0: never)
    pub deadline: u64,
    /// Bumped by whoever hands this thread the baton
    pub park: AtomicU32,
    /// Set while the thread is in `Shared::park`: it is not running, so a
    /// lock it holds stays held until the scheduler picks it again
    pub in_park: AtomicU32,
}

#[repr(C)]
pub struct ProcRec {
    pub state: u32,
    pub real_pid: i32,
    pub host: u32,
    pub parent: u32,
    /// Raw wait status, valid once `state` is `P_EXITED`
    pub exit_status: i32,
    /// Virtual time at which the run learned of the death (0: alive)
    pub died_at: u64,
    /// The parent has collected the exit with `waitpid`
    pub reaped: bool,
    /// Another guest sent it a fatal signal: it is as good as dead, though
    /// the launcher has not seen the exit yet
    pub killed: bool,
    /// Children that died since a thread of this process last took up the
    /// baton: its `SIGCHLD` is due
    pub child_deaths: u32,
    /// Bumped by every wake that comes from a thread the scheduler does not
    /// run. Such a wake is not serialized by the baton, so it can land
    /// between a thread's "would I block?" check and its blocking; a thread
    /// that sees the count move backs out and checks again.
    pub outside_wakes: u64,
    /// Some thread of this process runs outside the scheduler (GCD workers)
    pub has_outside_threads: bool,
    pub faults: Faults,
    /// Virtual time at which this life is crashed (0: not)
    pub crash_at: u64,
    /// The process registered to take this one's place, or `NO_PROC`
    pub replaced_by: u32,
    /// `SIGKILL` has been sent (or the process saw `killed` while attaching
    /// and went by itself)
    pub signalled: bool,
    /// Crashed while live, and the run has not yet waited out its real death
    pub unsettled: bool,
    /// Which run-file entry the launcher started this from (`NO_PROC` for a
    /// guest's own child, which only its parent could restart)
    pub spec: u32,
}

impl ProcRec {
    /// When this life is due to be crashed, if it still is
    fn pending_crash(&self) -> Option<u64> {
        (self.crash_at != 0 && self.state != P_EXITED && !self.killed).then_some(self.crash_at)
    }
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
    /// The run's one virtual clock, in nanoseconds since the run began.
    /// It moves only when a guest reads it or gives up the baton, and jumps
    /// to the next deadline when nothing can run.
    pub clock_ns: u64,
    pub current: u32,
    pub nthreads: u32,
    pub nprocs: u32,
    next_cond_seq: u64,
    /// Crash times come from their own stream, so that adding faults to a
    /// run does not shift the choice of threads
    fault_rng: Rng,
    /// Seed bisection: at the first hand-off at or after this virtual time
    /// (0: never) the scheduler's streams start over from `reseed_with`.
    /// Until then the run is the plain run of its seed.
    pub reseed_at: u64,
    pub reseed_with: u64,
    pub reseeded: bool,
    /// The launcher's `REWRITE_TRACE` and `REWRITE_MASK`, NUL-terminated.
    /// Here and not in the guests' environment: its size places a guest's
    /// stack, and a run must not move because it is being looked at.
    pub trace_path: [u8; DEBUG_PATH_LEN],
    pub mask_path: [u8; DEBUG_PATH_LEN],
    pub crashes_injected: u64,
    pub restarts: u64,
    /// Restarts that were due but found the process or thread table full;
    /// the launcher counts them
    pub restarts_refused: u64,
    unsignalled: u32,
    pub unsettled: u32,
    pub threads: [ThreadRec; MAX_THREADS],
    pub procs: [ProcRec; MAX_PROCS],
    pub net: netstate::Net,
}

#[repr(C)]
pub struct Shared {
    magic: u64,
    size: u64,
    /// Hook events left in the quantum. The stubs decrement it without the
    /// lock: only the baton holder runs guest code.
    counter: UnsafeCell<i64>,
    lock: AtomicU32,
    /// Real pid of the launcher, or 0 for a run that has none
    launcher: AtomicI32,
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
/// Exit status of a guest that found its launcher gone (`EX_SOFTWARE`)
pub const ORPHANED_EXIT: i32 = 70;
/// A parked thread looks for the launcher this often, in real time
const PARK_CHECK_NS: u64 = 1_000_000_000;

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
        s.fault_rng = Rng::seed_from_u64(seed ^ 0xFA17_FA17_FA17_FA17);
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

    /// The calling process runs the run: guests that outlive it exit.
    pub fn set_launcher(&self) {
        self.launcher
            .store(unsafe { libc::getpid() }, Ordering::Relaxed);
    }

    /// Exit if the launcher is gone. Nobody is left to reap this process,
    /// end the run or release a lock the launcher held, so a guest that
    /// stayed would park or spin forever.
    pub fn exit_if_orphaned(&self) {
        let launcher = self.launcher.load(Ordering::Relaxed);
        if launcher == 0 || launcher == unsafe { libc::getpid() } {
            return;
        }
        // EPERM would still mean it exists
        let gone = unsafe { libc::kill(launcher, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            unsafe { libc::_exit(ORPHANED_EXIT) };
        }
    }

    /// The lock word holds its owner's real pid. A process can be killed
    /// with the lock held (the launcher kills what is left at the end of a
    /// run; a crash is a `SIGKILL`), and a lock nobody can release would
    /// wedge everyone, the launcher included. A waiter that finds the owner
    /// gone takes the lock over. The state may be mid-update then; the
    /// alternative is a run that never ends.
    pub fn lock(&self) -> Guard<'_> {
        let me = unsafe { libc::getpid() } as u32;
        let mut spins = 0u32;
        while self
            .lock
            .compare_exchange_weak(0, me, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::hint::spin_loop();
            spins = spins.wrapping_add(1);
            // Critical sections are microseconds long: this many spins
            // means the holder may have died with the lock
            if spins.is_multiple_of(1 << 16) && self.take_over_from_the_dead(me) {
                break;
            }
            if spins.is_multiple_of(1 << 22) {
                self.exit_if_orphaned();
            }
        }
        Guard { shared: self }
    }

    fn take_over_from_the_dead(&self, me: u32) -> bool {
        let owner = self.lock.load(Ordering::Relaxed);
        if owner == 0 || owner == me {
            return false;
        }
        let pid = owner as libc::pid_t;
        let no_such = unsafe { libc::kill(pid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        // A zombie still answers `kill`. If it is our child (the launcher's
        // guests are), look without reaping: whoever reaps it still can.
        let dead_child = !no_such
            && unsafe {
                let mut info: libc::siginfo_t = std::mem::zeroed();
                let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
                libc::waitid(libc::P_PID, owner, &raw mut info, flags) == 0 && info.si_pid == pid
            };
        let gone = no_such || dead_child;
        gone && self
            .lock
            .compare_exchange(owner, me, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
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
    /// The replacement seed once the run's reseed time has passed (seed
    /// bisection). Read without the lock: it is set at a hand-off, and those
    /// who ask are scheduled threads, which run one at a time after it.
    pub fn reseeded_with(&self) -> Option<u64> {
        let state = self.state.get();
        unsafe { (*state).reseeded.then(|| (*state).reseed_with) }
    }

    pub fn is_parked(&self, id: usize) -> bool {
        let state = self.state.get();
        unsafe { (*state).threads[id].in_park.load(Ordering::Acquire) != 0 }
    }

    pub fn park(&self, id: usize, seen: u32) {
        let state = self.state.get();
        let in_park = unsafe { &(*state).threads[id].in_park };
        in_park.store(1, Ordering::Release);
        self.park_inner(id, seen);
        in_park.store(0, Ordering::Release);
    }

    fn park_inner(&self, id: usize, seen: u32) {
        let word = self.park_word(id);
        let atomic = unsafe { &*word.cast::<AtomicU32>() };
        // Optionally spin first: a baton that comes straight back saves the
        // sleep and the wake. Off by default; see the results write-up.
        for _ in 0..PARK_SPINS.load(Ordering::Relaxed) {
            if atomic.load(Ordering::Acquire) != seen {
                return;
            }
            std::hint::spin_loop();
        }
        while atomic.load(Ordering::Acquire) == seen {
            unsafe { __ulock_wait2(PARK_OP, word, u64::from(seen), PARK_CHECK_NS, 0) };
            self.exit_if_orphaned();
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
        p.replaced_by = NO_PROC;
        p.spec = NO_PROC;
        pid as u32
    }

    /// Fault settings of a process the launcher starts from run-file entry
    /// `spec`; its first crash time is drawn now.
    pub fn set_faults(&mut self, pid: u32, faults: Faults, spec: u32) {
        let p = &mut self.procs[pid as usize];
        p.faults = faults;
        p.spec = spec;
        let from = self.clock_ns;
        self.draw_crash(pid, from);
    }

    fn draw_crash(&mut self, pid: u32, life_starts: u64) {
        let f = self.procs[pid as usize].faults;
        self.procs[pid as usize].crash_at = if f.crash_hi_ns == 0 || f.crashes_left == 0 {
            0
        } else {
            let span = f.crash_hi_ns - f.crash_lo_ns + 1;
            (life_starts + f.crash_lo_ns + self.fault_rng.below(span)).max(1)
        };
    }

    /// Everything a violent death means to the run, done now rather than
    /// when the launcher notices: threads leave the schedule, virtual sockets
    /// close, the parent's `waitpid` wakes, the restart is registered. The
    /// caller sends the signals (`take_kills`) and then `settle_deaths`.
    pub fn crash(&mut self, victim: u32) {
        for t in self.live() {
            if t.pid == victim {
                t.state = T_EXITED;
                t.cond_key = 0;
                t.deadline = 0;
            }
        }
        let now = self.clock_ns;
        let p = &mut self.procs[victim as usize];
        p.killed = true;
        p.died_at = now;
        p.crash_at = 0;
        self.unsignalled += 1;
        // One that has not attached holds nothing the run could wait for
        if p.state == P_LIVE {
            p.unsettled = true;
            self.unsettled += 1;
        }
        let parent = p.parent;
        self.net.process_died(victim);
        self.wake_io();
        if parent != NO_PROC {
            self.procs[parent as usize].child_deaths += 1;
            self.wake_all(parent, WAIT_KEY);
        }
        self.register_restart(victim, KILLED_STATUS);
    }

    /// Hand each newly condemned real pid to `signal`. A process that has
    /// not attached has none; it goes by itself when it does.
    pub fn take_kills(&mut self, mut signal: impl FnMut(i32)) {
        if self.unsignalled == 0 {
            return;
        }
        self.unsignalled = 0;
        for p in &mut self.procs[..self.nprocs as usize] {
            if p.killed && !p.signalled && p.real_pid > 0 {
                p.signalled = true;
                signal(p.real_pid);
            }
        }
    }

    /// False while a crashed process is still really alive. What it held in
    /// the kernel (pipes, locks) is released at a moment of real time; the
    /// baton holder waits that out so waiters are woken at a fixed point.
    pub fn settle_deaths(&mut self) -> bool {
        if self.unsettled == 0 {
            return true;
        }
        let n = self.nprocs as usize;
        if self.procs[..n]
            .iter()
            .any(|p| p.unsettled && p.state != P_EXITED)
        {
            return false;
        }
        self.forget_unsettled();
        true
    }

    /// Stop waiting for crashed processes to really die.
    pub fn forget_unsettled(&mut self) {
        for p in &mut self.procs[..self.nprocs as usize] {
            p.unsettled = false;
        }
        self.unsettled = 0;
        self.wake_io();
    }

    pub fn restart_due(&self, pid: u32, status: i32) -> bool {
        let p = &self.procs[pid as usize];
        let wanted = match p.faults.restart {
            RESTART_ALWAYS => true,
            RESTART_ON_FAILURE => status != 0,
            _ => false,
        };
        wanted && p.spec != NO_PROC && p.faults.restarts_left > 0
    }

    fn has_room(&self) -> bool {
        (self.nprocs as usize) < MAX_PROCS && (self.nthreads as usize) < MAX_THREADS
    }

    /// Whether the launcher will be asked to start `pid` again after it
    /// died with `status`.
    pub fn will_restart(&self, pid: u32, status: i32) -> bool {
        self.procs[pid as usize].replaced_by != NO_PROC
            || (self.restart_due(pid, status) && self.has_room())
    }

    /// The next life of `victim`: a new process on the same host whose main
    /// thread is blocked until the restart delay is over. The real process
    /// is the launcher's to spawn; when it does cannot matter, since the
    /// thread is not runnable before its time and a baton handed to a
    /// process that has not attached yet waits for it.
    fn register_restart(&mut self, victim: u32, status: i32) {
        if self.procs[victim as usize].replaced_by != NO_PROC || !self.restart_due(victim, status) {
            return;
        }
        if !self.has_room() {
            return;
        }
        let old = &self.procs[victim as usize];
        let (host, spec, mut faults) = (old.host, old.spec, old.faults);
        let new = self.add_proc(host, NO_PROC);
        if faults.restarts_left != NO_LIMIT {
            faults.restarts_left -= 1;
        }
        let span = faults.restart_delay_hi_ns - faults.restart_delay_lo_ns + 1;
        let down_for = faults.restart_delay_lo_ns + self.fault_rng.below(span);
        let starts = self.clock_ns + down_for.max(1);
        let p = &mut self.procs[new as usize];
        p.faults = faults;
        p.spec = spec;
        self.procs[victim as usize].replaced_by = new;
        let main = self.add_thread(new);
        let t = &mut self.threads[main];
        t.state = T_BLOCKED;
        t.key = RESTART_KEY;
        t.deadline = starts;
        self.draw_crash(new, starts);
        self.restarts += 1;
    }

    /// Crash every process whose time has come, attached or not: the moment
    /// must not depend on when the real process got as far as attaching.
    fn inject_due_crashes(&mut self) {
        for pid in 0..self.nprocs {
            let due = self.procs[pid as usize].pending_crash();
            if due.is_none_or(|at| at > self.clock_ns) {
                continue;
            }
            let f = &mut self.procs[pid as usize].faults;
            if f.crashes_left != NO_LIMIT {
                f.crashes_left -= 1;
            }
            self.crash(pid);
            self.crashes_injected += 1;
        }
    }

    fn next_crash(&self) -> Option<u64> {
        self.procs[..self.nprocs as usize]
            .iter()
            .filter_map(ProcRec::pending_crash)
            .min()
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

    /// Make every thread of `pid` blocked on `key` runnable; returns how
    /// many there were.
    pub fn wake_all(&mut self, pid: u32, key: u64) -> usize {
        let mut woken = 0;
        for t in self.live() {
            if t.state == T_BLOCKED && t.pid == pid && t.key == key {
                t.state = T_RUNNABLE;
                woken += 1;
            }
        }
        woken
    }

    /// Let every thread parked for I/O readiness re-check. Kernel object
    /// state only changes when a guest acts, so doing this after each such
    /// act is exact.
    pub fn wake_io(&mut self) {
        for t in self.live() {
            if t.state == T_BLOCKED && t.key == IO_KEY {
                t.state = T_RUNNABLE;
            }
        }
    }

    pub fn wake_thread(&mut self, id: usize) {
        if self.threads[id].state == T_BLOCKED {
            self.threads[id].state = T_RUNNABLE;
        }
    }

    pub fn cond_enqueue(&mut self, id: usize, cond: u64) {
        let seq = self.next_cond_seq;
        self.next_cond_seq += 1;
        let t = &mut self.threads[id];
        t.signaled = false;
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
                .filter(|(_, t)| t.pid == pid && t.cond_key == cond && t.state != T_EXITED)
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

    /// A read of the clock: every read moves it a little, so busy-waits on
    /// the clock make progress.
    pub fn clock_read(&mut self) -> u64 {
        self.clock_ns += PER_READ_NS;
        self.clock_moved();
        self.clock_ns
    }

    /// Network traffic that has become due arrives.
    fn clock_moved(&mut self) {
        if self.net.advance(self.clock_ns) {
            self.wake_io();
        }
    }

    /// Release every blocked thread whose deadline has passed.
    fn expire_deadlines(&mut self) {
        let now = self.clock_ns;
        for t in self.live() {
            if t.state == T_BLOCKED && t.deadline != 0 && t.deadline <= now {
                t.deadline = 0;
                t.timed_out = true;
                t.state = T_RUNNABLE;
            }
        }
    }

    fn earliest_deadline(&mut self) -> Option<u64> {
        self.live()
            .iter()
            .filter(|t| t.state == T_BLOCKED && t.deadline != 0)
            .map(|t| t.deadline)
            .min()
    }

    /// Choose the next baton holder. Returns None when nothing can run.
    /// Deadlines that have passed are handled before the choice; when
    /// nothing is runnable the clock jumps to the earliest deadline.
    fn pick(&mut self) -> Option<usize> {
        if self.reseed_at != 0 && !self.reseeded && self.clock_ns >= self.reseed_at {
            self.reseeded = true;
            self.rng = Rng::seed_from_u64(self.reseed_with);
            self.fault_rng = Rng::seed_from_u64(self.reseed_with ^ 0xFA17_FA17_FA17_FA17);
        }
        self.clock_moved();
        self.inject_due_crashes();
        self.expire_deadlines();
        let mut runnable = self.live().iter().filter(|t| t.state == T_RUNNABLE).count();
        while runnable == 0 {
            // A timed waiter's deadline, a payload in flight or a crash,
            // whichever comes first
            let next = [
                self.earliest_deadline(),
                self.net.next_due(),
                self.next_crash(),
            ]
            .into_iter()
            .flatten()
            .min()?;
            self.clock_ns = self.clock_ns.max(next);
            self.clock_moved();
            self.inject_due_crashes();
            self.expire_deadlines();
            runnable = self.live().iter().filter(|t| t.state == T_RUNNABLE).count();
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
        // Every yield, not only a switch: a lone compute thread must still
        // let a sleeper's deadline pass.
        if me.is_some() {
            self.clock_ns += PER_YIELD_NS;
        }
        self.choose(me, site)
    }

    /// The choosing half of `hand_off`, for a caller whose state is already
    /// recorded: an idle scheduler looks again after threads it does not
    /// schedule (GCD workers) have had time to wake someone.
    pub fn choose(&mut self, me: Option<usize>, site: u64) -> Handoff {
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
    /// None when the process did not hold the baton.
    pub fn process_died(&mut self, pid: u32, status: i32) -> Option<Handoff> {
        self.procs[pid as usize].state = P_EXITED;
        self.procs[pid as usize].exit_status = status;
        if self.procs[pid as usize].died_at == 0 {
            self.procs[pid as usize].died_at = self.clock_ns.max(1);
        }
        // A crashed process died to the run when it was crashed; this call
        // comes at a moment of real time and must not wake anyone.
        if !self.procs[pid as usize].killed {
            // It held the baton, so this is a point of the schedule too
            self.register_restart(pid, status);
            self.procs[pid as usize].crash_at = 0;
            // Its descriptors are closed: peers may see EOF or EPIPE now
            self.net.process_died(pid);
            self.wake_io();
            let parent = self.procs[pid as usize].parent;
            if parent != NO_PROC {
                self.procs[parent as usize].child_deaths += 1;
                self.wake_all(parent, WAIT_KEY);
            }
        }
        // Whoever was last given the baton has it, whatever its state says:
        // a thread that found the run idle (and aborted over it, or was
        // polling) is already recorded as blocked.
        let current = self.current as usize;
        let held = (current < self.nthreads as usize && self.threads[current].pid == pid)
            .then_some(current);
        for t in self.live() {
            if t.pid == pid && t.state != T_EXITED {
                t.state = T_EXITED;
                t.cond_key = 0;
                t.deadline = 0;
            }
        }
        held.map(|id| self.hand_off(Some((id, T_EXITED, 0)), SITE_PROCESS_DIED))
    }

    /// An exited, unreaped child of `parent`: the one named, or the lowest.
    pub fn exited_child(&self, parent: u32, which: Option<u32>) -> Option<u32> {
        (0..self.nprocs).find(|&i| {
            let p = &self.procs[i as usize];
            p.parent == parent
                && (p.state == P_EXITED || p.killed)
                && !p.reaped
                && which.is_none_or(|w| w == i)
        })
    }

    /// Whether `parent` has a child it could still wait for.
    pub fn has_child(&self, parent: u32, which: Option<u32>) -> bool {
        (0..self.nprocs).any(|i| {
            let p = &self.procs[i as usize];
            p.parent == parent && !p.reaped && which.is_none_or(|w| w == i)
        })
    }

    /// Whether any live process has threads outside the scheduler, which
    /// could still wake someone when the run looks idle.
    pub fn any_outside_threads(&self) -> bool {
        self.procs[..self.nprocs as usize]
            .iter()
            .any(|p| p.state == P_LIVE && p.has_outside_threads)
    }

    /// True while some thread that has not exited belongs to a live process.
    pub fn any_alive(&self) -> bool {
        self.threads[..self.nthreads as usize]
            .iter()
            .any(|t| t.state != T_EXITED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shared mapping with a lock in it, as the run's state is
    fn shared_mapping() -> &'static Shared {
        let mem = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                Shared::SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANON,
                -1,
                0,
            )
        };
        assert_ne!(mem, libc::MAP_FAILED);
        unsafe { Shared::init(mem.cast(), 1, 1000, 10000) }
    }

    #[test]
    fn a_lock_whose_owner_died_is_taken_over() {
        let sh = shared_mapping();
        for reap_first in [true, false] {
            let child = unsafe { libc::fork() };
            if child == 0 {
                std::mem::forget(sh.lock());
                unsafe { libc::_exit(0) };
            }
            // The child is dead, reaped or still a zombie, with the lock
            let mut status = 0;
            let flags = if reap_first { 0 } else { libc::WNOWAIT };
            if reap_first {
                assert_eq!(
                    unsafe { libc::waitpid(child, &raw mut status, flags) },
                    child
                );
            } else {
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                let rc = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        child as u32,
                        &raw mut info,
                        libc::WEXITED | flags,
                    )
                };
                assert_eq!(rc, 0);
            }
            assert_eq!(sh.lock.load(Ordering::Relaxed), child as u32);
            let guard = sh.lock();
            assert_eq!(sh.lock.load(Ordering::Relaxed), std::process::id());
            drop(guard);
            assert_eq!(sh.lock.load(Ordering::Relaxed), 0);
            if !reap_first {
                unsafe { libc::waitpid(child, &raw mut status, 0) };
            }
        }
    }

    #[test]
    fn stubs_can_reach_the_counter() {
        let offset = std::mem::offset_of!(Shared, counter);
        assert_eq!(PRIVATE_SIZE + offset, COUNTER_OFFSET as usize);
    }
}
