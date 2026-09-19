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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
        Ok(Coordinator { shared, mem, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Delay for traffic between different hosts, in virtual time.
    pub fn set_net_latency(&self, ns: u64) {
        self.shared.lock().net.latency_ns = ns;
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

    /// Whether process `pid`, dead with `status`, gets another life.
    pub fn will_restart(&self, pid: u32, status: i32) -> bool {
        self.shared.lock().will_restart(pid, status)
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
        let handoff = self.shared.lock().hand_off(None, 0);
        if let Handoff::Switch { to, .. } = handoff {
            self.shared.unpark(to);
        }
    }

    /// Call after reaping `pid`. Returns true when the process held the
    /// baton and every surviving thread is blocked: a deadlock.
    pub fn process_died(&self, pid: u32, status: i32) -> bool {
        let mut s = self.shared.lock();
        let handoff = s.process_died(pid, status);
        // Passing the baton on may have crashed someone
        let (kills, n) = s.take_kills();
        let deadlock = matches!(handoff, Some(Handoff::Idle)) && s.any_alive();
        drop(s);
        for &victim in &kills[..n] {
            if victim > 0 {
                unsafe { libc::kill(victim, libc::SIGKILL) };
            }
        }
        if let Some(Handoff::Switch { to, .. }) = handoff {
            self.shared.unpark(to);
        }
        deadlock
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
        }
    }
}

impl Drop for Coordinator {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.mem, Shared::SIZE) };
        let _ = std::fs::remove_file(&self.path);
    }
}
