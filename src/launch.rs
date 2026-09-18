//! Spawn a (rewritten) guest with ASLR disabled and the supervisor dylib
//! injected, and collect the supervisor's end-of-run report.

use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};

use crate::coord::{Coordinator, Death, Totals};
use crate::shared;

/// Not in the libc crate; from `<spawn.h>` on Darwin.
const POSIX_SPAWN_DISABLE_ASLR: libc::c_short = 0x0100;

/// Environment variable naming the fd the supervisor writes its report to.
pub const REPORT_FD_VAR: &str = "REWRITE_REPORT_FD";

pub struct Launch {
    pub exe: PathBuf,
    pub args: Vec<OsString>,
    pub dylib: Option<PathBuf>,
    /// Extra `KEY=VALUE` pairs for the child's environment
    pub env: Vec<(String, String)>,
    pub disable_aslr: bool,
    /// Redirect the guest's stdout to this file (created or truncated)
    pub stdout: Option<PathBuf>,
    pub seed: u64,
    /// Hook events per quantum, inclusive range
    pub quantum: (u32, u32),
}

pub const DEFAULT_QUANTUM: (u32, u32) = (1000, 10000);

#[derive(Debug, Default, Clone)]
pub struct Report {
    pub fields: BTreeMap<String, String>,
}

impl Report {
    fn parse(text: &str) -> Self {
        let mut fields = BTreeMap::new();
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                fields.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        Report { fields }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    pub fn get_u64(&self, key: &str) -> Option<u64> {
        self.get(key)
            .and_then(|v| v.trim_start_matches("0x").parse().ok())
    }
}

#[derive(Debug)]
pub struct Outcome {
    /// Raw wait status; use the helpers below
    pub status: i32,
    pub report: Report,
}

impl Outcome {
    pub fn exit_code(&self) -> Option<i32> {
        libc::WIFEXITED(self.status).then(|| libc::WEXITSTATUS(self.status))
    }

    pub fn signal(&self) -> Option<i32> {
        libc::WIFSIGNALED(self.status).then(|| libc::WTERMSIG(self.status))
    }
}

fn cstring(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).expect("path contains NUL")
}

/// One guest of a run
pub struct Guest {
    pub exe: PathBuf,
    /// `argv[0]` when it should differ from `exe` (a manifest's own token
    /// rather than the rewritten file's path)
    pub argv0: Option<OsString>,
    pub args: Vec<OsString>,
    /// Index of the virtual host the process lives on
    pub host: u32,
    /// Redirect the guest's stdout to this file (created or truncated)
    pub stdout: Option<PathBuf>,
}

/// Several guests under one scheduler
pub struct Run {
    pub guests: Vec<Guest>,
    pub dylib: Option<PathBuf>,
    /// Extra `KEY=VALUE` pairs for every guest's environment
    pub env: Vec<(String, String)>,
    pub disable_aslr: bool,
    pub seed: u64,
    pub quantum: (u32, u32),
    /// Working directory for every guest
    pub cwd: Option<PathBuf>,
}

#[derive(Debug)]
pub struct RunOutcome {
    /// In the order of `Run::guests`
    pub guests: Vec<Outcome>,
    pub totals: Totals,
    /// The survivors were killed because every thread was blocked
    pub deadlock: bool,
}

struct Child {
    pid: libc::pid_t,
    report_fd: libc::c_int,
    status: Option<i32>,
    report: String,
}

extern "C" {
    fn posix_spawn_file_actions_addchdir_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        path: *const libc::c_char,
    ) -> libc::c_int;
}

fn spawn(run: &Run, guest: &Guest, extra_env: &[(String, String)]) -> io::Result<Child> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    unsafe {
        libc::fcntl(read_fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }

    let ours = [
        "DYLD_INSERT_LIBRARIES",
        REPORT_FD_VAR,
        shared::SHARED_VAR,
        shared::PROC_VAR,
    ];
    let mut env: Vec<CString> = std::env::vars_os()
        .filter(|(k, _)| !ours.iter().any(|o| k == o))
        .map(|(k, v)| {
            let mut s = k.into_vec();
            s.push(b'=');
            s.extend(v.into_vec());
            CString::new(s).unwrap()
        })
        .collect();
    if let Some(d) = &run.dylib {
        let mut s = b"DYLD_INSERT_LIBRARIES=".to_vec();
        s.extend(d.as_os_str().as_bytes());
        env.push(CString::new(s).unwrap());
    }
    env.push(CString::new(format!("{REPORT_FD_VAR}={write_fd}")).unwrap());
    for (k, v) in run.env.iter().chain(extra_env) {
        env.push(CString::new(format!("{k}={v}")).unwrap());
    }

    let exe = cstring(&guest.exe);
    let argv0 = match &guest.argv0 {
        Some(a) => CString::new(a.as_bytes()).unwrap(),
        None => exe.clone(),
    };
    let mut argv: Vec<CString> = vec![argv0];
    argv.extend(
        guest
            .args
            .iter()
            .map(|a| CString::new(a.as_bytes()).unwrap()),
    );
    let mut argv_ptrs: Vec<*mut libc::c_char> =
        argv.iter().map(|a| a.as_ptr().cast_mut()).collect();
    argv_ptrs.push(std::ptr::null_mut());
    let mut env_ptrs: Vec<*mut libc::c_char> = env.iter().map(|a| a.as_ptr().cast_mut()).collect();
    env_ptrs.push(std::ptr::null_mut());

    let mut pid: libc::pid_t = 0;
    let rc = unsafe {
        let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
        libc::posix_spawnattr_init(&raw mut attr);
        let flags = if run.disable_aslr {
            POSIX_SPAWN_DISABLE_ASLR
        } else {
            0
        };
        libc::posix_spawnattr_setflags(&raw mut attr, flags);
        let mut actions: libc::posix_spawn_file_actions_t = std::mem::zeroed();
        libc::posix_spawn_file_actions_init(&raw mut actions);
        let stdout_c = guest.stdout.as_deref().map(cstring);
        if let Some(p) = &stdout_c {
            libc::posix_spawn_file_actions_addopen(
                &raw mut actions,
                1,
                p.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                0o644,
            );
        }
        let cwd_c = run.cwd.as_deref().map(cstring);
        if let Some(p) = &cwd_c {
            posix_spawn_file_actions_addchdir_np(&raw mut actions, p.as_ptr());
        }
        let rc = libc::posix_spawn(
            &raw mut pid,
            exe.as_ptr(),
            &raw const actions,
            &raw const attr,
            argv_ptrs.as_ptr(),
            env_ptrs.as_ptr(),
        );
        libc::posix_spawn_file_actions_destroy(&raw mut actions);
        libc::posix_spawnattr_destroy(&raw mut attr);
        libc::close(write_fd);
        rc
    };
    if rc != 0 {
        unsafe { libc::close(read_fd) };
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(Child {
        pid,
        report_fd: read_fd,
        status: None,
        report: String::new(),
    })
}

impl Child {
    /// Reap if the process has ended; with `block`, wait for it.
    fn reap(&mut self, block: bool) -> bool {
        if self.status.is_some() {
            return true;
        }
        let mut status = 0;
        let flags = if block { 0 } else { libc::WNOHANG };
        if unsafe { libc::waitpid(self.pid, &raw mut status, flags) } != self.pid {
            return false;
        }
        self.status = Some(status);
        // The only writer is dead, so this reads to EOF without blocking
        // on the guest.
        let mut f = unsafe { std::fs::File::from_raw_fd(self.report_fd) };
        let _ = f.read_to_string(&mut self.report);
        true
    }

    fn kill(&self) {
        if self.status.is_none() {
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
        }
    }
}

/// Block until one of the unreaped children exits and return its index.
/// Watches the specific pids: `waitpid(-1)` would steal children from
/// other runs in the same launcher process.
fn wait_any(children: &mut [Child]) -> io::Result<usize> {
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        for (i, c) in children.iter_mut().enumerate() {
            if c.status.is_some() {
                continue;
            }
            let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
            ev.ident = c.pid as usize;
            ev.filter = libc::EVFILT_PROC;
            ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
            ev.fflags = libc::NOTE_EXIT;
            ev.udata = i as *mut libc::c_void;
            let rc = unsafe {
                libc::kevent(
                    kq,
                    &raw const ev,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
            // ESRCH: already a zombie
            if rc < 0 && c.reap(true) {
                return Ok(i);
            }
        }
        loop {
            let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
            let n =
                unsafe { libc::kevent(kq, std::ptr::null(), 0, &raw mut ev, 1, std::ptr::null()) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            let i = ev.udata as usize;
            if children[i].reap(true) {
                return Ok(i);
            }
        }
    })();
    unsafe { libc::close(kq) };
    result
}

/// Run every guest to completion under one scheduler. The launcher's
/// stdin/stdout/stderr are inherited unless a guest redirects stdout.
pub fn launch_run(run: &Run) -> io::Result<RunOutcome> {
    let coord = match &run.dylib {
        Some(_) => Some(Coordinator::create(run.seed, run.quantum)?),
        None => None,
    };
    let mut children: Vec<Child> = Vec::new();
    let result = supervise(run, coord.as_ref(), &mut children);
    if result.is_err() {
        for c in &mut children {
            c.kill();
            c.reap(true);
        }
    }
    let deadlock = result?;
    let totals = coord.as_ref().map(Coordinator::totals).unwrap_or_default();
    let guests = children
        .into_iter()
        .map(|c| {
            let mut report = Report::parse(&c.report);
            if coord.is_some() && !report.fields.is_empty() {
                // Run-wide values as of the end of the run, not of this
                // guest's exit
                for (k, v) in [
                    ("switches", totals.switches.to_string()),
                    ("expiries", totals.expiries.to_string()),
                    ("schedule_hash", format!("{:016x}", totals.schedule_hash)),
                ] {
                    report.fields.insert(k.into(), v);
                }
            }
            Outcome {
                status: c.status.unwrap_or(0),
                report,
            }
        })
        .collect();
    Ok(RunOutcome {
        guests,
        totals,
        deadlock,
    })
}

/// Returns whether the run ended in a deadlock.
fn supervise(
    run: &Run,
    coord: Option<&Coordinator>,
    children: &mut Vec<Child>,
) -> io::Result<bool> {
    for guest in &run.guests {
        let mut env = vec![
            ("REWRITE_SEED".to_string(), run.seed.to_string()),
            (
                "REWRITE_QUANTUM".to_string(),
                format!("{}..{}", run.quantum.0, run.quantum.1),
            ),
        ];
        if let Some(coord) = coord {
            let pid = coord.register(guest.host);
            debug_assert_eq!(pid as usize, children.len());
            env.push((
                shared::SHARED_VAR.into(),
                coord.path().to_string_lossy().into_owned(),
            ));
            env.push((shared::PROC_VAR.into(), pid.to_string()));
        }
        children.push(spawn(run, guest, &env)?);
    }
    let mut deadlock = false;
    if let Some(coord) = coord {
        let pids: Vec<u32> = (0..children.len() as u32).collect();
        let mut early = Vec::new();
        coord.wait_attached(
            &pids,
            |pid| {
                let died = children[pid as usize].reap(false);
                if died && !early.contains(&pid) {
                    early.push(pid);
                }
                died
            },
            std::time::Duration::from_secs(30),
        )?;
        for pid in early {
            let status = children[pid as usize].status.unwrap_or(0);
            coord.process_died(pid, status);
        }
        coord.start();
    }
    while children.iter().any(|c| c.status.is_none()) {
        let i = wait_any(children)?;
        let Some(coord) = coord else { continue };
        let status = children[i].status.unwrap_or(0);
        if coord.process_died(i as u32, status) == Death::Deadlock {
            deadlock = true;
            for c in children.iter_mut() {
                c.kill();
                c.reap(true);
            }
        }
    }
    Ok(deadlock)
}

/// Run one guest to completion.
pub fn launch(cfg: &Launch) -> io::Result<Outcome> {
    let run = Run {
        guests: vec![Guest {
            exe: cfg.exe.clone(),
            argv0: None,
            args: cfg.args.clone(),
            host: 0,
            stdout: cfg.stdout.clone(),
        }],
        dylib: cfg.dylib.clone(),
        env: cfg.env.clone(),
        disable_aslr: cfg.disable_aslr,
        seed: cfg.seed,
        quantum: cfg.quantum,
        cwd: None,
    };
    let mut out = launch_run(&run)?;
    Ok(out.guests.remove(0))
}

/// Where the supervisor dylib lives: next to the running `rewrite` binary.
pub fn default_dylib() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let p = dir.join("librewrite_supervisor.dylib");
    p.exists().then_some(p)
}
