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
}

/// What a guest's death meant for the run
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Death {
    /// Someone else holds the baton, or the run is over
    Quiet,
    /// The dead process held the baton and it was passed on
    HandedOn,
    /// The dead process held the baton and every surviving thread is blocked
    Deadlock,
}

static NEXT_FILE: AtomicU32 = AtomicU32::new(0);

impl Coordinator {
    pub fn create(seed: u64, quantum: (u32, u32)) -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "rewrite-shm-{}-{}",
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

    /// Fill the host table, in declaration order. Returns `name=address`
    /// pairs for `REWRITE_HOSTS`.
    pub fn add_hosts(&self, names: &[String]) -> String {
        let mut s = self.shared.lock();
        let mut out = Vec::new();
        for name in names {
            let i = s.net.add_host(name.as_bytes());
            let addr = std::net::Ipv4Addr::from(s.net.hosts[i as usize].addr);
            out.push(format!("{name}={addr}"));
        }
        out.join(",")
    }

    /// Register a process and its main thread before it is spawned, so
    /// process and thread ids follow launch order.
    pub fn register(&self, host: u32) -> u32 {
        let mut s = self.shared.lock();
        let pid = s.add_proc(host, shared::NO_PROC);
        s.add_thread(pid);
        pid
    }

    /// Wait until every process in `pids` has attached or `gone(pid)` says
    /// it died first. Fails if a guest mapped the state somewhere else.
    pub fn wait_attached(
        &self,
        pids: &[u32],
        mut gone: impl FnMut(u32) -> bool,
        timeout: Duration,
    ) -> io::Result<()> {
        let start = Instant::now();
        loop {
            let mut waiting = false;
            for &pid in pids {
                let (state, at) = {
                    let s = self.shared.lock();
                    let p = &s.procs[pid as usize];
                    (p.state, p.mapped_at)
                };
                if state == shared::P_STARTING {
                    waiting |= !gone(pid);
                } else if state == shared::P_LIVE && at != shared::MAP_ADDR as u64 {
                    return Err(io::Error::other(format!(
                        "guest {pid} mapped the scheduler state at {at:#x}, not {:#x}",
                        shared::MAP_ADDR
                    )));
                }
            }
            if !waiting {
                return Ok(());
            }
            if start.elapsed() > timeout {
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

    /// Call after reaping `pid`.
    pub fn process_died(&self, pid: u32, status: i32) -> Death {
        let mut s = self.shared.lock();
        match s.process_died(pid, status) {
            Handoff::Stay => Death::Quiet,
            Handoff::Switch { to, .. } => {
                drop(s);
                self.shared.unpark(to);
                Death::HandedOn
            }
            Handoff::Idle if s.any_alive() => Death::Deadlock,
            Handoff::Idle => Death::Quiet,
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
        }
    }
}

impl Drop for Coordinator {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.mem, Shared::SIZE) };
        let _ = std::fs::remove_file(&self.path);
    }
}
