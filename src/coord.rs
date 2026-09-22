//! Launcher side of the shared scheduler: creates the state file, registers
//! the initial processes, hands out the first baton, and passes the baton
//! on when a guest that held it dies.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::shared::{self, Handoff, Shared};

pub struct Coordinator {
    shared: &'static Shared,
    mem: *mut libc::c_void,
    path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Totals {
    pub switches: u64,
    pub expiries: u64,
    pub schedule_hash: u64,
    pub threads: u32,
    /// Connections and bytes that went through the virtual network, and
    /// connections that left it for the kernel's
    pub net_connections: u64,
    pub net_datagrams: u64,
    /// Datagrams nobody was bound to receive or that found a full ring
    pub net_dropped: u64,
    pub net_bytes: u64,
    pub net_passthrough: u64,
    pub crashes_injected: u64,
    pub restarts: u64,
    pub restarts_refused: u64,
    /// The virtual clock at the end of the run
    pub clock_ns: u64,
    /// When each process died, in virtual time (0: it did not)
    pub died_at: Vec<u64>,
    /// The run reached its `stop-after` time (0: it did not, or had none)
    pub stopped_at: u64,
    /// Which processes were killed by that stop
    pub stopped: Vec<bool>,
}

static NEXT_FILE: AtomicU32 = AtomicU32::new(0);

impl Coordinator {
    pub fn create(seed: u64, quantum: (u32, u32)) -> io::Result<Self> {
        // Fixed width: the guest's environment sits at the top of its stack,
        // so a name that is a digit longer in one run moves every stack
        // address the guest sees.
        // Not `temp_dir()`: its length differs between users and machines,
        // and this path is in every guest's environment.
        let path = Path::new("/tmp").join(format!(
            "rewrite-shm-{:010}-{:06}",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let cpath = CString::new(path.as_os_str().as_bytes()).expect("path contains NUL");
        let mem = unsafe {
            let fd = libc::open(
                cpath.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC,
                0o600,
            );
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let sized = libc::ftruncate(fd, Shared::SIZE as libc::off_t) == 0;
            let mem = if sized {
                libc::mmap(
                    std::ptr::null_mut(),
                    Shared::SIZE,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    0,
                )
            } else {
                libc::MAP_FAILED
            };
            let err = io::Error::last_os_error();
            libc::close(fd);
            if mem == libc::MAP_FAILED {
                let _ = std::fs::remove_file(&path);
                return Err(err);
            }
            mem
        };
        let shared = unsafe { Shared::init(mem.cast::<Shared>(), seed, quantum.0, quantum.1) };
        shared.set_launcher();
        Ok(Coordinator { shared, mem, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Seed bisection: the streams start over from `with` at time `at`.
    pub fn set_reseed(&self, at: u64, with: u64) {
        let mut s = self.shared.lock();
        s.reseed_at = at.saturating_add(1);
        s.reseed_with = with;
    }

    /// Hand our `REWRITE_TRACE` and `REWRITE_MASK` to the guests.
    pub fn set_debug_paths(&self) {
        let mut s = self.shared.lock();
        if let Ok(path) = std::env::var("REWRITE_TRACE") {
            shared::set_debug_path(&mut s.trace_path, &path);
        }
        if let Ok(path) = std::env::var("REWRITE_MASK") {
            shared::set_debug_path(&mut s.mask_path, &path);
        }
    }

    /// Delay for traffic between different hosts, in virtual time.
    pub fn set_net_latency(&self, ns: u64) {
        self.shared.lock().net.latency_ns = ns;
    }

    /// End the run when the virtual clock reaches `ns` (0: never).
    pub fn set_stop_at(&self, ns: u64) {
        self.shared.lock().stop_at_ns = ns;
    }

    /// The run is over at the virtual time it has reached: what still runs
    /// (the daemons, once every other process is done) is stopped at its
    /// next switch, reports, and exits, instead of being killed unheard.
    pub fn stop_now(&self) {
        let mut s = self.shared.lock();
        s.stop_at_ns = s.clock_ns.max(1);
    }

    /// Whether guests may connect to addresses outside the virtual network.
    pub fn set_outside_network(&self, allowed: bool) {
        self.shared.lock().net.outside_allowed = allowed;
    }

    /// Fill the host table, in declaration order: host `i` is `10.0.0.(i + 1)`.
    pub fn add_hosts(&self, names: &[String]) {
        let mut s = self.shared.lock();
        for name in names {
            s.net.add_host(name.as_bytes());
        }
    }

    /// Register a process and its main thread before it is spawned, so
    /// process and thread ids follow launch order.
    pub fn register(&self, host: u32, faults: shared::Faults, spec: u32) -> u32 {
        let mut s = self.shared.lock();
        let pid = s.add_proc(host, shared::NO_PROC);
        s.add_thread(pid);
        s.set_faults(pid, faults, spec);
        pid
    }

    /// Whether process `pid`, dead with `status`, gets another life. Ask
    /// once per death: a restart the full tables rule out is counted here.
    pub fn will_restart(&self, pid: u32, status: i32) -> bool {
        let mut s = self.shared.lock();
        let will = s.will_restart(pid, status);
        if !will && s.restart_due(pid, status) {
            s.restarts_refused += 1;
        }
        will
    }

    /// The process registered to take `pid`'s place, for us to spawn.
    pub fn replacement_of(&self, pid: u32) -> Option<u32> {
        let next = self.shared.lock().procs[pid as usize].replaced_by;
        (next != shared::NO_PROC).then_some(next)
    }

    /// Wait until each of the first `n` processes has attached, or
    /// `gone(index)` says it died first. A guest that cannot place the
    /// state at its fixed address dies saying so.
    pub fn wait_attached(&self, n: u32, mut gone: impl FnMut(u32) -> bool) -> io::Result<()> {
        let start = Instant::now();
        loop {
            let mut waiting = false;
            for pid in 0..n {
                let starting = self.shared.lock().procs[pid as usize].state == shared::P_STARTING;
                waiting |= starting && !gone(pid);
            }
            if !waiting {
                return Ok(());
            }
            if start.elapsed() > Duration::from_secs(30) {
                return Err(io::Error::other(
                    "a guest never attached to the scheduler (is the supervisor dylib injected?)",
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Hand out the first baton.
    pub fn start(&self) {
        // Every guest's main thread has attached; wait until each is parked
        // too, so that none is still finishing its start-up while another
        // runs guest code
        let began = std::time::Instant::now();
        loop {
            let all_parked = {
                let s = self.shared.lock();
                s.threads[..s.nthreads as usize]
                    .iter()
                    .all(|t| t.in_park.load(std::sync::atomic::Ordering::Acquire) != 0)
            };
            if all_parked || began.elapsed() > Duration::from_secs(10) {
                break;
            }
            std::thread::yield_now();
        }
        let handoff = self.shared.lock().hand_off(None, 0);
        if let Handoff::Switch { to, .. } = handoff {
            self.shared.unpark(to);
        }
    }

    /// The run ends with this death: note when, without passing the baton.
    pub fn note_last_death(&self, pid: u32) {
        let mut s = self.shared.lock();
        let now = s.clock_ns.max(1);
        let p = &mut s.procs[pid as usize];
        if p.died_at == 0 {
            p.died_at = now;
        }
    }

    /// Call after reaping `pid`. Returns true when the process held the
    /// baton and every surviving thread is blocked: a deadlock.
    pub fn process_died(&self, pid: u32, status: i32) -> bool {
        let mut s = self.shared.lock();
        let handoff = s.process_died(pid, status);
        // Passing the baton on may have crashed someone
        s.take_kills(|victim| unsafe {
            libc::kill(victim, libc::SIGKILL);
        });
        let deadlock = matches!(handoff, Some(Handoff::Idle)) && s.any_alive() && !s.stopped;
        drop(s);
        if let Some(Handoff::Switch { to, .. }) = handoff {
            self.shared.unpark(to);
        }
        deadlock
    }

    /// Remove the System V objects the guests made: a run leaves nothing
    /// in the machine's namespace.
    pub fn remove_ipc_objects(&self) {
        let s = self.shared.lock();
        for &(kind, id, _) in &s.ipc_objects[..s.nipc_objects as usize] {
            unsafe {
                if kind == crate::shared::IPC_SHM {
                    libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
                } else {
                    libc::semctl(id, 0, libc::IPC_RMID);
                }
            }
        }
        for name in &s.pshm_names[..s.npshm_names as usize] {
            let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
            if let Ok(c) = CString::new(&name[..end]) {
                unsafe { libc::shm_unlink(c.as_ptr()) };
            }
        }
    }

    pub fn totals(&self) -> Totals {
        let s = self.shared.lock();
        Totals {
            switches: s.switches,
            expiries: s.expiries,
            schedule_hash: s.trace_hash,
            threads: s.nthreads,
            net_connections: s.net.connections,
            net_datagrams: s.net.datagrams,
            net_dropped: s.net.dropped,
            net_bytes: s.net.bytes,
            net_passthrough: s.net.passthrough,
            crashes_injected: s.crashes_injected,
            restarts: s.restarts,
            restarts_refused: s.restarts_refused,
            clock_ns: s.clock_ns,
            died_at: s.procs[..s.nprocs as usize]
                .iter()
                .map(|p| p.died_at)
                .collect(),
            stopped_at: if s.stopped { s.stop_at_ns } else { 0 },
            stopped: s.procs[..s.nprocs as usize]
                .iter()
                .map(|p| p.stopped)
                .collect(),
        }
    }
}

impl Drop for Coordinator {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.mem, Shared::SIZE) };
        let _ = std::fs::remove_file(&self.path);
    }
}
