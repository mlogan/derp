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
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

use crate::rng::Rng;

#[path = "netstate.rs"]
pub mod netstate;

pub const MAGIC: u64 = 0x0031_4448_5357_5252;

/// What each secondary random stream's seed is the run's seed xored with
const FAULT_STREAM: u64 = 0xFA17_FA17_FA17_FA17;
const PAUSE_STREAM: u64 = 0x9A05_E9A0_5E9A_05E9;
const PCT_STREAM: u64 = 0x0C70_0C70_0C70_0C70;
/// Mixed into the trace hash with the id of a thread that is paused, or
/// that a PCT change point demotes
const PAUSE_MARK: u64 = 0x9A05_E000;
const PCT_MARK: u64 = 0x0C70_0000;
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
/// Next to it, the entry a counter-read trampoline calls
pub const COUNTER_ENTRY_OFFSET: u32 = 8;
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
    /// A shutdown is injected this far into each life, drawn from the range
    /// (both 0: none): `shutdown_signal` is sent, and the process is
    /// crashed if it is still alive `grace_ns` later
    pub shutdown_lo_ns: u64,
    pub shutdown_hi_ns: u64,
    /// `NO_LIMIT`, or how many shutdowns are left to inject
    pub shutdowns_left: u32,
    pub shutdown_signal: u8,
    pub grace_ns: u64,
}

/// How often the scheduler pauses the thread it chose, and for how long
/// (virtual time, drawn log-uniformly from the range)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pauses {
    /// `num / den` of scheduling choices; `num` 0: never
    pub num: u32,
    pub den: u32,
    pub lo_ns: u64,
    pub hi_ns: u64,
}

impl Pauses {
    pub const OFF: Pauses = Pauses {
        num: 0,
        den: 1,
        lo_ns: 10_000,
        hi_ns: 100_000_000,
    };
}

/// The uniform pick: each choice draws among the runnable threads
pub const SCHED_RANDOM: u8 = 0;
/// PCT (Burckhardt et al., ASPLOS 2010): the runnable thread of highest
/// priority runs; at `depth - 1` points the holder drops below everyone
pub const SCHED_PCT: u8 = 1;
pub const MAX_PCT_DEPTH: u32 = 16;

/// How the next baton holder is chosen
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scheduler {
    pub kind: u8,
    /// PCT's d: it finds a bug that needs d ordering constraints with
    /// probability at least 1/(n * k^(d-1)) per run
    pub pct_depth: u32,
    /// The change points fall in `[0, horizon)` of virtual time, and
    /// priorities hold until it is over
    pub pct_horizon_ns: u64,
    /// A runnable thread passed over for this long runs once anyway, so a
    /// thread spinning on a lower one's progress cannot livelock the run
    pub pct_starve_ns: u64,
}

impl Scheduler {
    pub const RANDOM: Scheduler = Scheduler {
        kind: SCHED_RANDOM,
        pct_depth: 3,
        pct_horizon_ns: 0,
        pct_starve_ns: 50_000_000,
    };
}

pub const DEBUG_PATH_LEN: usize = 1024;

/// What the cache puts between a program's file name and the key of its
/// rewritten copy
pub const CACHE_TAG: &str = ".rw6-";
pub const MAX_IPC_OBJECTS: usize = 256;
pub const IPC_SHM: u32 = 1;
pub const IPC_SEM: u32 = 2;

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

/// False for a process that does not exist, and for a zombie, whoever's
/// child it is: a zombie still answers `kill`, but runs no more code.
/// `proc_pidinfo` fails with `ESRCH` for both.
#[must_use]
pub fn process_lives(pid: libc::pid_t) -> bool {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let got =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    // Someone else's process is not ours to inspect, but it exists
    got == size || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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
/// Payload: the executable's path. Sent once a process holds the baton for
/// the first time, so the launcher knows every process's program, a
/// guest's own children and a new image after `execve` included.
pub const MSG_IMAGE: u8 = 4;

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
/// What a baton hand-off costs in virtual time, unless the run says
/// otherwise (`switch-cost`). Threads run one at a time, so a run of
/// hundreds of threads shares one clock: a costly hand-off makes their
/// work slow in virtual time and fires the programs' own timeouts. Kept
/// low, a program that times its own work finds it very fast instead.
pub const DEFAULT_SWITCH_NS: u64 = 10_000;
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
    /// Thread-directed signals (`pthread_kill` from another scheduled
    /// thread) not yet delivered: a bit per signal, raised when this
    /// thread next takes the baton up
    pub sig_pending: u64,
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
    /// Virtual time until which the scheduler passes this thread over while
    /// another can run (0: not paused)
    pub paused_until: u64,
    /// Signal handlers run on this thread, so a wait can tell that one
    /// interrupted it (`EINTR`)
    pub handlers_run: u64,
    /// PCT: higher runs first. Drawn above `MAX_PCT_DEPTH` at creation; a
    /// change point sets it to its own number, below every drawn one.
    pub priority: u64,
    /// PCT: since when this runnable thread has been passed over (0: it
    /// was chosen last time, or has not been runnable since)
    pub waiting_since: u64,
}

#[repr(C)]
pub struct ProcRec {
    pub state: u32,
    pub real_pid: i32,
    pub host: u32,
    pub parent: u32,
    /// Raw wait status, valid once `state` is `P_EXITED`
    pub exit_status: i32,
    /// Its interval timer (`setitimer(ITIMER_REAL)`, `alarm`): the virtual
    /// time its SIGALRM is due (0: none) and the interval that re-arms it
    pub alarm_at: u64,
    pub alarm_interval: u64,
    /// Signals other guests sent it, not yet delivered: a bit per signal.
    /// Delivered to the next thread of the process to take the baton up
    /// that does not block them (`sched::deliver_pending_signals`).
    pub sig_pending: u64,
    /// How many of each signal guests have sent it, itself included, for
    /// `EVFILT_SIGNAL` watches
    pub sig_counts: [u32; 32],
    /// Mach port of a thread of this process that handed the baton on
    /// from its teardown and may still be running destructors (0: none);
    /// the next holder in this process waits for it to be gone
    pub exiting_port: u32,
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
    /// The child whose death is the latest of them, for the handler's
    /// `siginfo`
    pub last_dead_child: u32,
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
    /// Virtual time at which this life is sent its shutdown signal (0: not)
    pub shutdown_at: u64,
    /// The end of the grace period of a shutdown under way (0: none)
    pub kill_at: u64,
    /// Sent its shutdown signal: however it ends, a restart policy other
    /// than `never` brings it back, as after an upgrade
    pub shut_down: bool,
    /// The process registered to take this one's place, or `NO_PROC`
    pub replaced_by: u32,
    /// `SIGKILL` has been sent (or the process saw `killed` while attaching
    /// and went by itself)
    pub signalled: bool,
    /// Crashed while live, and the run has not yet waited out its real death
    pub unsettled: bool,
    /// Killed because the run reached `State::stop_at_ns`, not by a fault
    pub stopped: bool,
    /// Which run-file entry the launcher started this from (`NO_PROC` for a
    /// guest's own child, which only its parent could restart)
    pub spec: u32,
}

impl ProcRec {
    fn alive(&self) -> bool {
        self.state != P_EXITED && !self.killed
    }

    /// When this life is due to be crashed, if it still is
    fn pending_crash(&self) -> Option<u64> {
        (self.crash_at != 0 && self.alive()).then_some(self.crash_at)
    }

    /// When this life is due to be sent its shutdown signal, or crashed at
    /// the end of its grace period, if it still is
    fn pending_shutdown(&self) -> Option<u64> {
        let due = [self.shutdown_at, self.kill_at]
            .into_iter()
            .filter(|&t| t != 0)
            .min()?;
        self.alive().then_some(due)
    }
}

#[repr(C)]
pub struct State {
    pub rng: Rng,
    pub quantum_lo: u32,
    pub quantum_hi: u32,
    /// Virtual time each hand-off adds to the clock
    pub switch_ns: u64,
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
    /// Pauses come from their own stream too: with pauses off, the run is
    /// the one it was before they existed
    pause_rng: Rng,
    /// A scheduling choice pauses the chosen thread with probability
    /// `pause_num / pause_den` (`pause_num` 0: never), for a time drawn
    /// log-uniformly from `pause_lo_ns..=pause_hi_ns`
    pub pause_num: u32,
    pub pause_den: u32,
    pub pause_lo_ns: u64,
    pub pause_hi_ns: u64,
    pub pauses: u64,
    pub scheduler: u8,
    pct_rng: Rng,
    pct_starve_ns: u64,
    /// Past it the uniform pick takes over, as in Coyote's fair PCT: a
    /// program that spins on a lower thread's progress would otherwise go
    /// at the pace of the starvation bound for the rest of the run
    pct_horizon_ns: u64,
    /// Virtual times of the change points, ascending; the first
    /// `pct_points` are valid and `pct_next` is the next to come
    pct_change_at: [u64; MAX_PCT_DEPTH as usize],
    pct_points: u32,
    pct_next: u32,
    pub pct_changes: u64,
    pub pct_starved: u64,
    /// Seed bisection: one more than the virtual time (0: never) from whose
    /// first hand-off on every stream starts over from `reseed_with`. Until
    /// then the run is the plain run of its seed.
    pub reseed_at: u64,
    pub reseed_with: u64,
    pub reseeded: bool,
    /// The launcher's `REWRITE_TRACE` and `REWRITE_MASK`, NUL-terminated.
    /// Here and not in the guests' environment: its size places a guest's
    /// stack, and a run must not move because it is being looked at.
    pub trace_path: [u8; DEBUG_PATH_LEN],
    pub mask_path: [u8; DEBUG_PATH_LEN],
    pub crashes_injected: u64,
    pub shutdowns_injected: u64,
    /// Shutdowns whose grace period ran out: the process was crashed
    pub shutdowns_forced: u64,
    /// System V objects the guests created, for the launcher to remove
    /// when the run ends: (1 shared memory, 2 semaphore set), id. Guests
    /// get private objects (`IPC_PRIVATE`) whatever key they name, since
    /// keys are a namespace of the whole machine and what a run leaves
    /// there would make the next run's tries differ.
    pub ipc_objects: [(u32, i32, u64); MAX_IPC_OBJECTS],
    pub nipc_objects: u32,
    /// POSIX shared memory names the guests made (as given to the kernel,
    /// with the run's prefix), NUL-padded, for the launcher to unlink
    pub pshm_names: [[u8; 32]; MAX_IPC_OBJECTS],
    pub npshm_names: u32,
    pub restarts: u64,
    /// Restarts that were due but found the process or thread table full;
    /// the launcher counts them
    pub restarts_refused: u64,
    /// Virtual time at which the run is over (0: when its processes are):
    /// every process still alive is killed at that point of the schedule
    pub stop_at_ns: u64,
    pub stopped: bool,
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
    /// `owner_word` of the holder, 0 when free
    lock: AtomicU64,
    /// Real pid of the launcher, or 0 for a run that has none
    launcher: AtomicI32,
    /// Wakes a signal handler could not make (see `defer_wake`): pairs of
    /// (process index, key + 1), 0 when empty
    deferred: [AtomicU64; 2 * DEFERRED_WAKES],
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
        // Wakes a signal handler left to us, with the state consistent again
        let sh = self.shared;
        for i in 0..DEFERRED_WAKES {
            let key = sh.deferred[2 * i + 1].load(Ordering::Acquire);
            if key == 0 {
                continue;
            }
            let pid = sh.deferred[2 * i].load(Ordering::Acquire) as u32;
            sh.deferred[2 * i + 1].store(0, Ordering::Release);
            let state = unsafe { &mut *sh.state.get() };
            if key - 1 == DEFERRED_IO {
                state.wake_io();
            } else {
                state.wake_all(pid, key - 1);
            }
        }
        sh.lock.store(0, Ordering::Release);
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
    fn pthread_mach_thread_np(t: libc::pthread_t) -> u32;
}

const DEFERRED_WAKES: usize = 8;
/// The key `defer_wake` stores for an I/O wake
pub const DEFERRED_IO: u64 = u64::MAX - 1;

/// What the lock word says of this thread: pid above, Mach thread port
/// below. A word naming this thread is this thread inside the lock, which
/// only a signal handler can see.
fn owner_word() -> u64 {
    let port = unsafe { pthread_mach_thread_np(libc::pthread_self()) };
    (unsafe { libc::getpid() } as u64) << 32 | u64::from(port)
}

fn owner_pid(word: u64) -> libc::pid_t {
    (word >> 32) as libc::pid_t
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
        s.fault_rng = Rng::seed_from_u64(seed ^ FAULT_STREAM);
        s.pause_rng = Rng::seed_from_u64(seed ^ PAUSE_STREAM);
        s.pct_rng = Rng::seed_from_u64(seed ^ PCT_STREAM);
        s.quantum_lo = quantum_lo;
        s.quantum_hi = quantum_hi;
        s.switch_ns = DEFAULT_SWITCH_NS;
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
    /// Real pid of the launcher, 0 in a run without one.
    pub fn launcher_pid(&self) -> i32 {
        self.launcher.load(Ordering::Relaxed)
    }

    pub fn exit_if_orphaned(&self) {
        let launcher = self.launcher.load(Ordering::Relaxed);
        if launcher == 0 || launcher == unsafe { libc::getpid() } {
            return;
        }
        if !process_lives(launcher) {
            unsafe { libc::_exit(ORPHANED_EXIT) };
        }
    }

    /// The word names the owner's pid and thread: a process killed with the
    /// lock held (end of run, a crash) would wedge everyone, so a waiter
    /// takes it over from a dead owner, state mid-update or not; and a
    /// signal handler on the holding thread must not wait for itself.
    pub fn lock(&self) -> Guard<'_> {
        self.lock_unless_reentrant().unwrap_or_else(|| {
            let msg = "the scheduler lock was taken again by the thread holding it";
            unsafe { libc::write(2, msg.as_ptr().cast(), msg.len()) };
            std::process::abort()
        })
    }

    /// None when this thread holds the lock already: a signal handler
    /// interrupted the critical section.
    pub fn lock_unless_reentrant(&self) -> Option<Guard<'_>> {
        let me = owner_word();
        let mut spins = 0u32;
        while self
            .lock
            .compare_exchange_weak(0, me, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            if self.lock.load(Ordering::Relaxed) == me {
                return None;
            }
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
        Some(Guard { shared: self })
    }

    /// A wake that a signal handler on the lock's holder cannot make now:
    /// the holder makes it as it leaves the lock. Lost if all slots are
    /// taken, which eight handlers deep is not.
    pub fn defer_wake(&self, pid: u32, key: u64) {
        for i in 0..DEFERRED_WAKES {
            let slot = &self.deferred[2 * i + 1];
            if slot
                .compare_exchange(0, key.wrapping_add(1), Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                self.deferred[2 * i].store(u64::from(pid), Ordering::Release);
                return;
            }
        }
    }

    /// Before a new image's first lock: `execve` keeps the pid and kills
    /// the other threads, so a word naming this pid is one of them caught
    /// inside the lock, and nothing of this image can hold it yet.
    pub fn reclaim_after_exec(&self) {
        let word = self.lock.load(Ordering::Relaxed);
        if word != 0 && owner_pid(word) == unsafe { libc::getpid() } {
            let _ = self
                .lock
                .compare_exchange(word, 0, Ordering::Acquire, Ordering::Relaxed);
        }
    }

    fn take_over_from_the_dead(&self, me: u64) -> bool {
        let owner = self.lock.load(Ordering::Relaxed);
        if owner == 0 || owner_pid(owner) == owner_pid(me) {
            return false;
        }
        let gone = !process_lives(owner_pid(owner));
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

    /// Sleep until thread `id`'s park word differs from `seen`.
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

    /// When this life of `pid` is crashed and shut down, if it is. A run
    /// without shutdowns draws as it did before they existed.
    fn draw_crash(&mut self, pid: u32, life_starts: u64) {
        let f = self.procs[pid as usize].faults;
        self.procs[pid as usize].crash_at = if f.crash_hi_ns == 0 || f.crashes_left == 0 {
            0
        } else {
            let span = f.crash_hi_ns - f.crash_lo_ns + 1;
            (life_starts + f.crash_lo_ns + self.fault_rng.below(span)).max(1)
        };
        self.procs[pid as usize].shutdown_at = if f.shutdown_hi_ns == 0 || f.shutdowns_left == 0 {
            0
        } else {
            let span = f.shutdown_hi_ns - f.shutdown_lo_ns + 1;
            (life_starts + f.shutdown_lo_ns + self.fault_rng.below(span)).max(1)
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
        p.shutdown_at = 0;
        p.kill_at = 0;
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
            self.procs[parent as usize].last_dead_child = victim;
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
            RESTART_ON_FAILURE => status != 0 || p.shut_down,
            _ => false,
        };
        wanted && !self.stopped && p.spec != NO_PROC && p.faults.restarts_left > 0
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

    /// Shutdowns whose time has come: the signal is sent as by another
    /// guest. Grace periods that are over: the process is crashed.
    fn inject_due_shutdowns(&mut self) {
        let now = self.clock_ns;
        for pid in 0..self.nprocs {
            let p = &self.procs[pid as usize];
            if !p.alive() {
                continue;
            }
            if p.kill_at != 0 && p.kill_at <= now {
                self.crash(pid);
                self.shutdowns_forced += 1;
                continue;
            }
            if p.shutdown_at == 0 || p.shutdown_at > now {
                continue;
            }
            let p = &mut self.procs[pid as usize];
            let f = &mut p.faults;
            if f.shutdowns_left != NO_LIMIT {
                f.shutdowns_left -= 1;
            }
            let (sig, grace) = (c_int::from(f.shutdown_signal), f.grace_ns);
            p.shutdown_at = 0;
            p.kill_at = now + grace.max(1);
            p.shut_down = true;
            self.post_signal(pid, sig);
            self.wake_io();
            self.shutdowns_injected += 1;
        }
    }

    /// The stop that is still ahead, if the run has one.
    fn pending_stop(&self) -> Option<u64> {
        (self.stop_at_ns != 0 && !self.stopped).then_some(self.stop_at_ns)
    }

    /// The run is over: every process alive is marked as stopped, and none
    /// is restarted. A point of the schedule like an injected crash, so it
    /// falls at the same place every run. Every thread of a stopped process
    /// is made runnable: the first of them to take the baton up writes the
    /// process's report and ends it (`sched::die_if_stopped`), so a stopped
    /// process is heard from, which a killed one is not.
    fn stop_run(&mut self) {
        self.stopped = true;
        for pid in 0..self.nprocs as usize {
            let p = &mut self.procs[pid];
            if p.state != P_EXITED && !p.killed {
                p.stopped = true;
            }
        }
        for t in &mut self.threads[..self.nthreads as usize] {
            if t.state != T_EXITED && self.procs[t.pid as usize].stopped {
                t.state = T_RUNNABLE;
            }
        }
    }

    fn next_crash(&self) -> Option<u64> {
        let procs = &self.procs[..self.nprocs as usize];
        let crashes = procs.iter().filter_map(ProcRec::pending_crash);
        crashes
            .chain(procs.iter().filter_map(ProcRec::pending_shutdown))
            .min()
    }

    /// Register a thread of `pid`; it starts parked and runnable. Slots are
    /// never reused, so ids are a function of creation order.
    pub fn add_thread(&mut self, pid: u32) -> usize {
        let id = self.nthreads as usize;
        assert!(id < MAX_THREADS, "too many threads in one run");
        self.nthreads += 1;
        let priority = if self.scheduler == SCHED_PCT {
            u64::from(MAX_PCT_DEPTH) + 1 + (self.pct_rng.next_u64() >> 1)
        } else {
            0
        };
        let t = &mut self.threads[id];
        t.state = T_RUNNABLE;
        t.pid = pid;
        t.priority = priority;
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

    /// Another guest sent `sig` to `pid`: pending until a thread of it
    /// that does not block the signal takes the baton up. Every blocked
    /// thread of it is woken to look, as the kernel interrupts whichever
    /// thread takes a signal; waits that the signal does not end go back
    /// to waiting. A life still down before its restart is left alone.
    pub fn post_signal(&mut self, pid: u32, sig: c_int) {
        let p = &mut self.procs[pid as usize];
        p.sig_pending |= 1 << sig;
        p.sig_counts[sig as usize] += 1;
        for t in self.live() {
            if t.pid == pid && t.state == T_BLOCKED && t.key != RESTART_KEY {
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
    /// Interval timers that are due: their SIGALRM is pending, and one
    /// blocked thread of the process is woken to take it.
    fn fire_alarms(&mut self) {
        let now = self.clock_ns;
        for pid in 0..self.nprocs as usize {
            let p = &mut self.procs[pid];
            if p.state == P_EXITED || p.alarm_at == 0 || p.alarm_at > now {
                continue;
            }
            p.alarm_at = if p.alarm_interval == 0 {
                0
            } else {
                now + p.alarm_interval
            };
            p.sig_pending |= 1 << libc::SIGALRM;
            p.sig_counts[libc::SIGALRM as usize] += 1;
            if let Some(t) = self
                .live()
                .iter_mut()
                .find(|t| t.pid == pid as u32 && t.state == T_BLOCKED)
            {
                t.state = T_RUNNABLE;
            }
        }
    }

    fn next_alarm(&self) -> Option<u64> {
        self.procs[..self.nprocs as usize]
            .iter()
            .filter(|p| p.state != P_EXITED && p.alarm_at != 0)
            .map(|p| p.alarm_at)
            .min()
    }

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
        if self.reseed_at != 0 && !self.reseeded && self.clock_ns + 1 >= self.reseed_at {
            self.reseeded = true;
            self.rng = Rng::seed_from_u64(self.reseed_with);
            self.fault_rng = Rng::seed_from_u64(self.reseed_with ^ FAULT_STREAM);
            self.pause_rng = Rng::seed_from_u64(self.reseed_with ^ PAUSE_STREAM);
            self.pct_rng = Rng::seed_from_u64(self.reseed_with ^ PCT_STREAM);
        }
        if self.pending_stop().is_some_and(|at| self.clock_ns >= at) {
            self.stop_run();
        }
        self.clock_moved();
        self.inject_due_crashes();
        self.inject_due_shutdowns();
        self.expire_deadlines();
        self.fire_alarms();
        let mut runnable = self.live().iter().filter(|t| t.state == T_RUNNABLE).count();
        while runnable == 0 {
            // Nobody left: the run is over where it is, not at its stop time
            if !self.any_alive() {
                return None;
            }
            // A timed waiter's deadline, a payload in flight, a crash or the
            // run's stop, whichever comes first
            let next = [
                self.earliest_deadline(),
                self.net.next_due(),
                self.next_crash(),
                self.pending_stop(),
                self.next_alarm(),
            ]
            .into_iter()
            .flatten()
            .min()?;
            self.clock_ns = self.clock_ns.max(next);
            if self.pending_stop().is_some_and(|at| self.clock_ns >= at) {
                self.stop_run();
            }
            self.clock_moved();
            self.inject_due_crashes();
            self.inject_due_shutdowns();
            self.expire_deadlines();
            self.fire_alarms();
            runnable = self.live().iter().filter(|t| t.state == T_RUNNABLE).count();
        }
        // A pause holds a thread back only while another can run
        let now = self.clock_ns;
        let unpaused = self
            .live()
            .iter()
            .filter(|t| t.state == T_RUNNABLE && t.paused_until <= now)
            .count();
        self.pct_change_points();
        if unpaused == 0 {
            return Some(self.choose_candidate(false, runnable));
        }
        let chosen = self.choose_candidate(true, unpaused);
        if unpaused == 1 || !self.draw_pause() {
            return Some(chosen);
        }
        let until = now
            + self
                .pause_rng
                .log_uniform(self.pause_lo_ns, self.pause_hi_ns);
        self.threads[chosen].paused_until = until;
        self.pauses += 1;
        self.trace_hash = fnv(fnv(self.trace_hash, PAUSE_MARK), chosen as u64);
        Some(self.choose_candidate(true, unpaused - 1))
    }

    /// Before a run's first thread: PCT's change points are drawn over the
    /// horizon, and threads added from now on get priorities.
    pub fn set_scheduler(&mut self, sched: Scheduler) {
        self.scheduler = sched.kind;
        if sched.kind != SCHED_PCT {
            return;
        }
        self.pct_starve_ns = sched.pct_starve_ns;
        self.pct_horizon_ns = sched.pct_horizon_ns.max(1);
        let n = sched.pct_depth.clamp(1, MAX_PCT_DEPTH) - 1;
        for i in 0..n as usize {
            self.pct_change_at[i] = self.pct_rng.below(sched.pct_horizon_ns.max(1));
        }
        self.pct_change_at[..n as usize].sort_unstable();
        self.pct_points = n;
        self.pct_next = 0;
    }

    /// Change points the clock has reached: the thread that last held the
    /// baton drops to the point's number, below every drawn priority. Later
    /// points give higher numbers, as in PCT.
    fn pct_change_points(&mut self) {
        while self.pct_next < self.pct_points
            && self.pct_change_at[self.pct_next as usize] <= self.clock_ns
        {
            self.pct_next += 1;
            let holder = self.current as usize;
            if holder < self.nthreads as usize {
                self.threads[holder].priority = u64::from(self.pct_next);
                self.pct_changes += 1;
                self.trace_hash = fnv(fnv(self.trace_hash, PCT_MARK), holder as u64);
            }
        }
    }

    /// The next holder among the `count` runnable threads (paused ones
    /// passed over when `honour_pauses`).
    fn choose_candidate(&mut self, honour_pauses: bool, count: usize) -> usize {
        if self.scheduler != SCHED_PCT || self.clock_ns >= self.pct_horizon_ns {
            return self.nth_candidate(honour_pauses, count);
        }
        let now = self.clock_ns;
        let starve = self.pct_starve_ns;
        let eligible =
            |t: &ThreadRec| t.state == T_RUNNABLE && (!honour_pauses || t.paused_until <= now);
        let live = &self.threads[..self.nthreads as usize];
        // The longest passed over, if past the bound; else the highest
        // priority. Ties go to the lowest id.
        let starved = live
            .iter()
            .enumerate()
            .filter(|(_, t)| eligible(t) && t.waiting_since != 0 && now - t.waiting_since >= starve)
            .min_by_key(|(i, t)| (t.waiting_since, *i))
            .map(|(i, _)| i);
        let chosen = starved.unwrap_or_else(|| {
            live.iter()
                .enumerate()
                .filter(|(_, t)| eligible(t))
                .max_by_key(|(i, t)| (t.priority, std::cmp::Reverse(*i)))
                .map(|(i, _)| i)
                .expect("a candidate")
        });
        if starved.is_some() {
            self.pct_starved += 1;
        }
        for (i, t) in self.live().iter_mut().enumerate() {
            if i == chosen {
                t.waiting_since = 0;
            } else if t.state == T_RUNNABLE && t.waiting_since == 0 {
                t.waiting_since = now.max(1);
            }
        }
        chosen
    }

    pub fn set_pauses(&mut self, p: Pauses) {
        (self.pause_num, self.pause_den) = (p.num, p.den);
        (self.pause_lo_ns, self.pause_hi_ns) = (p.lo_ns, p.hi_ns);
    }

    /// Whether this choice pauses the chosen thread
    fn draw_pause(&mut self) -> bool {
        self.pause_num != 0
            && self.pause_rng.below(u64::from(self.pause_den)) < u64::from(self.pause_num)
    }

    /// One of the `count` runnable threads, drawn uniformly; paused ones
    /// are passed over when `honour_pauses`.
    fn nth_candidate(&mut self, honour_pauses: bool, count: usize) -> usize {
        let now = self.clock_ns;
        let n = self.rng.below(count as u64) as usize;
        self.live()
            .iter()
            .enumerate()
            .filter(|(_, t)| t.state == T_RUNNABLE && (!honour_pauses || t.paused_until <= now))
            .nth(n)
            .map(|(i, _)| i)
            .expect("as many candidates as counted")
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
            self.clock_ns += self.switch_ns;
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
            self.procs[pid as usize].shutdown_at = 0;
            self.procs[pid as usize].kill_at = 0;
            // Its descriptors are closed: peers may see EOF or EPIPE now
            self.net.process_died(pid);
            self.wake_io();
            let parent = self.procs[pid as usize].parent;
            if parent != NO_PROC {
                self.procs[parent as usize].child_deaths += 1;
                self.procs[parent as usize].last_dead_child = pid;
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
        shared_mapping_seeded(1)
    }

    fn shared_mapping_seeded(seed: u64) -> &'static Shared {
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
        unsafe { Shared::init(mem.cast(), seed, 1000, 10000) }
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
            assert_eq!(owner_pid(sh.lock.load(Ordering::Relaxed)), child);
            let guard = sh.lock();
            assert_eq!(sh.lock.load(Ordering::Relaxed), owner_word());
            drop(guard);
            assert_eq!(sh.lock.load(Ordering::Relaxed), 0);
            if !reap_first {
                unsafe { libc::waitpid(child, &raw mut status, 0) };
            }
        }
    }

    /// After `execve` the lock word may name our own pid: a thread of the old
    /// image died inside the lock.
    #[test]
    fn a_lock_word_naming_our_own_pid_is_stale_at_start() {
        let sh = shared_mapping();
        // A thread of ours that is gone
        sh.lock
            .store(u64::from(std::process::id()) << 32 | 7, Ordering::Relaxed);
        sh.reclaim_after_exec();
        let guard = sh.lock();
        assert_eq!(sh.lock.load(Ordering::Relaxed), owner_word());
        drop(guard);
        // Held by someone else: not ours to reclaim
        sh.lock.store(1 << 32 | 7, Ordering::Relaxed);
        sh.reclaim_after_exec();
        assert_eq!(sh.lock.load(Ordering::Relaxed), 1 << 32 | 7);
        sh.lock.store(0, Ordering::Relaxed);
    }

    /// A signal handler on the thread inside the lock: it must not wait for
    /// itself, and a wake it wants is made when the holder lets go.
    #[test]
    fn the_holding_thread_sees_its_own_lock_and_defers_a_wake() {
        let sh = shared_mapping();
        let guard = sh.lock();
        assert!(sh.lock_unless_reentrant().is_none());
        // Someone to wake: a thread blocked on key 42 of process 0
        let id = {
            let state = unsafe { &mut *sh.state.get() };
            let p = state.add_proc(0, NO_PROC);
            let id = state.add_thread(p);
            state.threads[id].state = T_BLOCKED;
            state.threads[id].key = 42;
            id
        };
        sh.defer_wake(0, 42);
        assert_eq!(unsafe { &*sh.state.get() }.threads[id].state, T_BLOCKED);
        drop(guard);
        assert_eq!(unsafe { &*sh.state.get() }.threads[id].state, T_RUNNABLE);
        assert_eq!(sh.lock.load(Ordering::Relaxed), 0);
    }

    /// A run's state with `threads` runnable threads in one process
    fn state_with_threads(seed: u64, threads: usize) -> &'static mut State {
        let sh = shared_mapping_seeded(seed);
        let s = unsafe { &mut *sh.state.get() };
        let p = s.add_proc(0, NO_PROC);
        for _ in 0..threads {
            s.add_thread(p);
        }
        s
    }

    /// Picks, with the clock moving 10µs before each as a hand-off moves it
    fn picks(s: &mut State, n: usize) -> Vec<usize> {
        (0..n)
            .map(|_| {
                s.clock_ns += 10_000;
                s.pick().unwrap()
            })
            .collect()
    }

    #[test]
    fn without_pauses_the_choice_is_a_uniform_draw_from_the_run_stream() {
        let s = state_with_threads(5, 3);
        let mut rng = Rng::seed_from_u64(5);
        let expected: Vec<usize> = (0..200).map(|_| rng.below(3) as usize).collect();
        assert_eq!(picks(s, 200), expected);
        assert_eq!(s.pauses, 0);
    }

    #[test]
    fn a_lone_runnable_thread_is_never_paused() {
        let s = state_with_threads(5, 1);
        (s.pause_num, s.pause_den, s.pause_lo_ns, s.pause_hi_ns) = (1, 1, 1_000_000, 1_000_000);
        assert!(picks(s, 50).iter().all(|&t| t == 0));
        assert_eq!(s.pauses, 0);
    }

    #[test]
    fn a_paused_thread_is_passed_over_until_its_pause_ends() {
        let s = state_with_threads(9, 3);
        (s.pause_num, s.pause_den, s.pause_lo_ns, s.pause_hi_ns) = (1, 4, 50_000, 5_000_000);
        let mut paused_seen = 0;
        for _ in 0..2000 {
            s.clock_ns += 10_000;
            let before = s.pauses;
            let now = s.clock_ns;
            let unpaused: Vec<usize> = (0..3)
                .filter(|&i| s.threads[i].paused_until <= now)
                .collect();
            let chosen = s.pick().unwrap();
            if unpaused.is_empty() {
                continue;
            }
            assert!(
                s.threads[chosen].paused_until <= now,
                "t{chosen} is paused while {unpaused:?} were not"
            );
            if s.pauses > before {
                paused_seen += 1;
                assert!(unpaused.len() > 1, "paused the only candidate");
            }
        }
        assert!(paused_seen > 20, "{paused_seen}");
        assert_eq!(s.pauses, paused_seen);
    }

    #[test]
    fn with_every_thread_paused_one_still_runs() {
        let s = state_with_threads(9, 2);
        s.threads[0].paused_until = u64::MAX;
        s.threads[1].paused_until = u64::MAX;
        (s.pause_num, s.pause_den, s.pause_lo_ns, s.pause_hi_ns) = (1, 1, 1_000, 1_000);
        assert_eq!(picks(s, 20).len(), 20);
        assert_eq!(s.pauses, 0);
    }

    #[test]
    fn pauses_are_a_function_of_the_seed() {
        let run = |seed| {
            let s = state_with_threads(seed, 4);
            (s.pause_num, s.pause_den, s.pause_lo_ns, s.pause_hi_ns) = (1, 10, 10_000, 100_000_000);
            (picks(s, 3000), s.pauses, s.trace_hash)
        };
        assert_eq!(run(11), run(11));
        assert_ne!(run(11).0, run(12).0);
    }

    /// PCT over `threads` threads of one process, change points as given
    fn pct_state(seed: u64, threads: usize, depth: u32, horizon_ns: u64) -> &'static mut State {
        let sh = shared_mapping_seeded(seed);
        let s = unsafe { &mut *sh.state.get() };
        s.set_scheduler(Scheduler {
            kind: SCHED_PCT,
            pct_depth: depth,
            pct_horizon_ns: horizon_ns,
            pct_starve_ns: 50_000_000,
        });
        let p = s.add_proc(0, NO_PROC);
        for _ in 0..threads {
            s.add_thread(p);
        }
        s
    }

    /// `pick` as `choose` uses it: the chosen thread becomes the holder
    fn pct_picks(s: &mut State, n: usize) -> Vec<usize> {
        (0..n)
            .map(|_| {
                s.clock_ns += 10_000;
                let t = s.pick().unwrap();
                s.current = t as u32;
                t
            })
            .collect()
    }

    #[test]
    fn pct_runs_the_highest_priority_thread_until_a_change_point() {
        let s = pct_state(4, 3, 1, 10_000_000_000);
        let top = (0..3).max_by_key(|&i| s.threads[i].priority).unwrap();
        assert!((0..3).all(|i| s.threads[i].priority > u64::from(MAX_PCT_DEPTH)));
        assert!(pct_picks(s, 100).iter().all(|&t| t == top));
        assert_eq!(s.pct_changes, 0);
        // The holder blocks: the next one down runs
        s.threads[top].state = T_BLOCKED;
        let second = (0..3)
            .filter(|&i| i != top)
            .max_by_key(|&i| s.threads[i].priority)
            .unwrap();
        assert!(pct_picks(s, 10).iter().all(|&t| t == second));
    }

    #[test]
    fn a_change_point_drops_the_holder_below_everyone() {
        // Depth 2: one change point somewhere in the first millisecond
        let s = pct_state(4, 3, 2, 1_000_000);
        // Priorities hold past the span the change point was drawn from
        s.pct_horizon_ns = u64::MAX;
        let top = (0..3).max_by_key(|&i| s.threads[i].priority).unwrap();
        let picks = pct_picks(s, 200);
        assert_eq!(s.pct_changes, 1);
        assert_eq!(s.threads[top].priority, 1);
        let switch = picks.iter().position(|&t| t != top).unwrap();
        assert!(picks[..switch].iter().all(|&t| t == top));
        assert!(picks[switch..].iter().all(|&t| t == picks[switch]));
        // The change point fell where the switch is
        assert!(switch <= 100, "{switch}");
    }

    #[test]
    fn a_thread_passed_over_too_long_runs_once() {
        let s = pct_state(4, 2, 1, 10_000_000_000);
        let top = (0..2).max_by_key(|&i| s.threads[i].priority).unwrap();
        let low = 1 - top;
        // 50ms of 10µs hand-offs: the low thread runs once per bound
        let picks = pct_picks(s, 20_000);
        let low_runs: Vec<usize> = (0..picks.len()).filter(|&i| picks[i] == low).collect();
        assert_eq!(low_runs.len(), 3, "{low_runs:?}");
        assert!(
            low_runs.windows(2).all(|w| w[1] - w[0] == 5001),
            "{low_runs:?}"
        );
        assert_eq!(s.pct_starved, 3);
    }

    #[test]
    fn past_the_horizon_the_uniform_pick_takes_over() {
        // Depth 1, horizon 1ms: 100 picks of priorities, then uniform
        let s = pct_state(4, 3, 1, 1_000_000);
        let top = (0..3).max_by_key(|&i| s.threads[i].priority).unwrap();
        let picks = pct_picks(s, 1000);
        assert!(picks[..99].iter().all(|&t| t == top));
        for t in 0..3 {
            let n = picks[100..].iter().filter(|&&p| p == t).count();
            assert!((200..400).contains(&n), "t{t} ran {n} of 900");
        }
    }

    #[test]
    fn pct_is_a_function_of_the_seed_and_leaves_the_quantum_stream_alone() {
        let run = |seed| {
            let s = pct_state(seed, 5, 4, 10_000_000);
            (pct_picks(s, 2000), s.pct_changes, s.trace_hash)
        };
        assert_eq!(run(21), run(21));
        assert_ne!(run(21).0, run(22).0);
        // The random scheduler draws from the run's stream; PCT does not
        let s = pct_state(21, 3, 3, 10_000_000_000);
        pct_picks(s, 500);
        let mut rng = Rng::seed_from_u64(21);
        assert_eq!(s.rng.next_u64(), rng.next_u64());
    }

    /// A run with one live process of two threads blocked in a sleep, set
    /// to be shut down 1ms into its life with a 5ms grace period
    fn shutdown_state(restart: u8) -> &'static mut State {
        let sh = shared_mapping_seeded(3);
        let s = unsafe { &mut *sh.state.get() };
        let p = s.add_proc(0, NO_PROC);
        s.procs[p as usize].state = P_LIVE;
        for _ in 0..2 {
            let t = s.add_thread(p);
            s.threads[t].state = T_BLOCKED;
            s.threads[t].key = SLEEP_KEY;
            s.threads[t].deadline = 10_000_000_000;
        }
        let faults = Faults {
            restart,
            restart_delay_lo_ns: 1_000_000,
            restart_delay_hi_ns: 1_000_000,
            restarts_left: NO_LIMIT,
            shutdown_lo_ns: 1_000_000,
            shutdown_hi_ns: 1_000_000,
            shutdowns_left: 1,
            shutdown_signal: libc::SIGTERM as u8,
            grace_ns: 5_000_000,
            ..Faults::default()
        };
        s.set_faults(p, faults, 0);
        s
    }

    #[test]
    fn a_shutdown_sends_its_signal_and_wakes_the_process() {
        let s = shutdown_state(RESTART_NEVER);
        assert_eq!(s.procs[0].shutdown_at, 1_000_000);
        // Nothing runnable: the clock jumps to the shutdown
        let t = s.pick().unwrap();
        assert_eq!(s.clock_ns, 1_000_000);
        assert_eq!(s.threads[t].pid, 0);
        assert_eq!(s.procs[0].sig_pending, 1 << libc::SIGTERM);
        assert_eq!(s.procs[0].sig_counts[libc::SIGTERM as usize], 1);
        assert!(s.threads[..2].iter().all(|t| t.state == T_RUNNABLE));
        assert_eq!(s.procs[0].kill_at, 6_000_000);
        assert_eq!(s.shutdowns_injected, 1);
        assert!(!s.procs[0].killed);
    }

    #[test]
    fn a_process_still_alive_after_its_grace_period_is_crashed() {
        let s = shutdown_state(RESTART_ON_FAILURE);
        s.pick().unwrap();
        // It handles the signal and goes back to sleep
        for t in &mut s.threads[..2] {
            t.state = T_BLOCKED;
        }
        // Crashed at 6ms; the next life's thread runs after its 1ms delay
        assert!(s.pick().is_some());
        assert_eq!(s.procs[0].died_at, 6_000_000);
        assert_eq!(s.clock_ns, 7_000_000);
        assert!(s.procs[0].killed);
        assert_eq!(s.shutdowns_forced, 1);
        assert_eq!(s.restarts, 1);
        // The next life has no shutdown left to inject
        let next = s.procs[0].replaced_by as usize;
        assert_eq!(s.procs[next].shutdown_at, 0);
    }

    #[test]
    fn a_clean_exit_during_a_shutdown_counts_as_a_failure_for_restarts() {
        let s = shutdown_state(RESTART_ON_FAILURE);
        s.pick().unwrap();
        s.current = 0;
        s.process_died(0, 0);
        assert_eq!(s.restarts, 1);
        assert_eq!(s.shutdowns_forced, 0);
        // Without a shutdown, exit 0 is not a failure
        let s = shutdown_state(RESTART_ON_FAILURE);
        s.process_died(0, 0);
        assert_eq!(s.restarts, 0);
    }

    #[test]
    fn stubs_can_reach_the_counter() {
        let offset = std::mem::offset_of!(Shared, counter);
        assert_eq!(PRIVATE_SIZE + offset, COUNTER_OFFSET as usize);
    }
}
