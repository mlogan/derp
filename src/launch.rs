//! Run guests under one scheduler: spawn each with ASLR disabled and the
//! supervisor dylib injected, serve their requests over the run's socket
//! (rewriting spawned programs, watching their children), pass the baton on
//! when a process that held it dies, and collect the reports.

use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::coord::{Coordinator, Totals};
use crate::shared;

/// Not in the libc crate; from `<spawn.h>` on Darwin.
const POSIX_SPAWN_DISABLE_ASLR: libc::c_short = 0x0100;

pub struct Launch {
    pub exe: PathBuf,
    pub args: Vec<OsString>,
    pub dylib: Option<PathBuf>,
    pub disable_aslr: bool,
    /// Bytes of address space for each guest's heap
    pub heap_size: u64,
    /// Redirect the guest's stdout to this file (created or truncated)
    pub stdout: Option<PathBuf>,
    pub stderr: Option<PathBuf>,
    pub seed: u64,
    /// Hook events per quantum, inclusive range
    pub quantum: (u32, u32),
    /// See `Run::stop_at_ns`
    pub stop_at_ns: u64,
    /// See `Run::wall_limit_ms`
    pub wall_limit_ms: u64,
    /// Inject the dylib without scheduling: see `Run::passive`
    pub passive: bool,
    /// See `Run::rewrite`
    pub rewrite: Option<crate::rewrite::Options>,
}

/// Tells the supervisor to set up the stubs' region and nothing else
pub const PASSIVE_VAR: &str = "REWRITE_PASSIVE";

/// Address space of a guest's heap unless the run says otherwise. Only
/// touched pages cost memory; it bounds what a guest can have live, and a
/// seeded layout wants room to scatter blocks in.
pub const DEFAULT_HEAP: u64 = 32 << 30;

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
        self.get(key).and_then(|v| v.parse().ok())
    }
}

#[derive(Debug)]
pub struct Outcome {
    /// Raw wait status; use the helpers below
    pub status: i32,
    pub report: Report,
    /// The executable the process ran and the original it was rewritten
    /// from; None for a run without the supervisor
    pub image: Option<PathBuf>,
    pub program: Option<PathBuf>,
    /// Killed because the run reached its stop time: not a failure
    pub stopped: bool,
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
    /// `KEY=VALUE` pairs for this guest only, after `Run::env`
    pub env: Vec<(String, String)>,
    /// Redirect the guest's stdout to this file (created or truncated)
    pub stdout: Option<PathBuf>,
    pub stderr: Option<PathBuf>,
    /// Working directory: the guest's host directory in a manifest run
    pub cwd: Option<PathBuf>,
    /// Killed when every guest that is not a daemon has exited
    pub daemon: bool,
    /// Crashes to inject into it and whether it is started again
    pub faults: shared::Faults,
}

/// Several guests under one scheduler
pub struct Run {
    pub guests: Vec<Guest>,
    /// Virtual hosts by name; `Guest::host` indexes this. Host `i` gets
    /// the address `10.0.0.(i + 1)`.
    pub hosts: Vec<String>,
    pub dylib: Option<PathBuf>,
    /// Whether guests inherit the launcher's environment underneath what
    /// the run sets. Run-file runs do not: the shell's variables would be
    /// an unrecorded input, and their total length moves the guest's stack.
    pub inherit_env: bool,
    pub disable_aslr: bool,
    /// Bytes of address space for each guest's heap
    pub heap_size: u64,
    pub seed: u64,
    pub quantum: (u32, u32),
    /// Rewritten binaries need the dylib for the region their stubs
    /// address. Passive runs get that and no scheduler, which measures the
    /// stubs alone.
    pub passive: bool,
    /// How to rewrite programs the guests spawn; without it they are
    /// expected to be rewritten already.
    pub rewrite: Option<crate::rewrite::Options>,
    /// Virtual-time delay for traffic between different hosts
    pub net_latency_ns: u64,
    /// Virtual time each baton hand-off costs
    pub switch_ns: u64,
    /// Seed bisection: (virtual time, replacement seed)
    pub reseed: Option<(u64, u64)>,
    /// Virtual time at which the run is over whatever is still running
    /// (0: never): for servers that never exit by themselves
    pub stop_at_ns: u64,
    /// Real time, in milliseconds, after which the run is over whatever is
    /// still running (0: never). For measuring a run against a native one,
    /// which has no virtual clock to stop at.
    pub wall_limit_ms: u64,
    /// Whether guests may connect outside the virtual network
    pub outside_network: bool,
}

#[derive(Debug)]
pub struct RunOutcome {
    /// Every process of the run in registration order: `Run::guests`
    /// first, then the ones guests spawned
    pub guests: Vec<Outcome>,
    /// How many of `guests` the launcher started itself
    pub initial: usize,
    /// Which of the initial guests were daemons, killed at the end
    pub daemons: Vec<bool>,
    /// For each of `guests`, the index in `Run::guests` it was started
    /// from; a restarted process shares it with its earlier lives
    pub specs: Vec<Option<usize>>,
    pub totals: Totals,
    /// The real-time limit ended the run
    pub wall_limited: bool,
    /// The survivors were killed because every thread was blocked
    pub deadlock: bool,
}

/// A process of the run as the launcher sees it, indexed like the shared
/// process table
struct Tracked {
    /// Which of `Run::guests` this is a life of; None for a guest's child
    spec: Option<usize>,
    pid: libc::pid_t,
    status: Option<i32>,
    /// CPU time it used, user and system, in nanoseconds, once reaped
    cpu: Option<(u64, u64)>,
    /// Killed by the real-time limit
    wall_limited: bool,
    report: String,
    /// What it ran, once it said (`MSG_IMAGE`): the executable, and the
    /// original that was rewritten into it
    image: Option<PathBuf>,
    program: Option<PathBuf>,
}

extern "C" {
    fn posix_spawn_file_actions_addchdir_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        path: *const libc::c_char,
    ) -> libc::c_int;
}

/// `guest_sock`, when given, becomes the guest's `COORD_FD`.
fn spawn(
    run: &Run,
    guest: &Guest,
    extra_env: &[(String, String)],
    guest_sock: Option<libc::c_int>,
    keep_stdout: bool,
) -> io::Result<libc::pid_t> {
    let ours = [
        "DYLD_INSERT_LIBRARIES",
        PASSIVE_VAR,
        shared::EXTERNAL_VAR,
        shared::SHARED_VAR,
        shared::PROC_VAR,
    ];
    // What the run sets replaces what we inherited: `getenv` returns the
    // first match, so a second `HOME` further down would never be seen.
    let set: Vec<&String> = guest.env.iter().chain(extra_env).map(|(k, _)| k).collect();
    let mut env: Vec<CString> = std::env::vars_os()
        .filter(|_| run.inherit_env)
        .filter(|(k, _)| !ours.iter().any(|o| k == o))
        .filter(|(k, _)| !set.iter().any(|s| k == s.as_str()))
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
    // The supervisor's own debugging switches, which a guest that does not
    // inherit our environment would otherwise never see
    if !run.inherit_env {
        for name in ["REWRITE_PARK_SPINS"] {
            if let Ok(value) = std::env::var(name) {
                env.push(CString::new(format!("{name}={value}")).unwrap());
            }
        }
    }
    for (k, v) in guest.env.iter().chain(extra_env) {
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
        let stderr_c = guest.stderr.as_deref().map(cstring);
        for (fd, path) in [(1, &stdout_c), (2, &stderr_c)] {
            let Some(p) = path else { continue };
            libc::posix_spawn_file_actions_addopen(
                &raw mut actions,
                fd,
                p.as_ptr(),
                // Appending, so that lives of one entry (and children that
                // outlive a crashed one) never write over each other
                libc::O_WRONLY
                    | libc::O_CREAT
                    | libc::O_APPEND
                    | if keep_stdout { 0 } else { libc::O_TRUNC },
                0o644,
            );
        }
        let cwd_c = guest.cwd.as_deref().map(cstring);
        if let Some(p) = &cwd_c {
            posix_spawn_file_actions_addchdir_np(&raw mut actions, p.as_ptr());
        }
        if let Some(sock) = guest_sock {
            libc::posix_spawn_file_actions_adddup2(&raw mut actions, sock, shared::COORD_FD);
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
        rc
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(pid)
}

impl Tracked {
    fn new(pid: libc::pid_t, spec: Option<usize>) -> Self {
        Tracked {
            spec,
            pid,
            status: None,
            cpu: None,
            wall_limited: false,
            report: String::new(),
            image: None,
            program: None,
        }
    }

    /// Our own child (we reap it) rather than a guest's
    fn ours(&self) -> bool {
        self.spec.is_some()
    }

    /// Reap one of our own children if it has ended; with `block`, wait.
    fn reap(&mut self, block: bool) -> bool {
        if self.status.is_some() {
            return true;
        }
        let mut status = 0;
        let flags = if block { 0 } else { libc::WNOHANG };
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::wait4(self.pid, &raw mut status, flags, &raw mut usage) } != self.pid {
            return false;
        }
        self.status = Some(status);
        let ns = |t: libc::timeval| {
            (t.tv_sec.max(0) as u64) * 1_000_000_000 + (t.tv_usec.max(0) as u64) * 1000
        };
        self.cpu = Some((ns(usage.ru_utime), ns(usage.ru_stime)));
        true
    }

    fn kill(&self) {
        if self.status.is_none() {
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
        }
    }
}

/// kqueue over the run's processes and its socket. Exits are watched per
/// pid: `waitpid(-1)` would steal children from other runs in the same
/// launcher process, and guests' own children are not ours to wait for.
struct Events {
    kq: libc::c_int,
}

enum Event {
    Exited {
        index: usize,
        status: i32,
    },
    Socket,
    /// The real-time limit
    Timer,
}

const SOCKET_UDATA: usize = usize::MAX;
const TIMER_UDATA: usize = usize::MAX - 1;

impl Events {
    fn new() -> io::Result<Self> {
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Events { kq })
    }

    fn add(&self, ident: usize, filter: i16, fflags: u32, udata: usize) -> bool {
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = ident;
        ev.filter = filter;
        ev.flags = libc::EV_ADD;
        ev.fflags = fflags;
        ev.udata = udata as *mut libc::c_void;
        let rc = unsafe {
            libc::kevent(
                self.kq,
                &raw const ev,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        rc >= 0
    }

    /// False when the process is already gone.
    fn watch_exit(&self, pid: libc::pid_t, index: usize) -> bool {
        self.add(
            pid as usize,
            libc::EVFILT_PROC,
            libc::NOTE_EXIT | libc::NOTE_EXITSTATUS,
            index,
        )
    }

    fn watch_socket(&self, fd: libc::c_int) {
        self.add(fd as usize, libc::EVFILT_READ, 0, SOCKET_UDATA);
    }

    fn watch_timer(&self, ms: u64) {
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = TIMER_UDATA;
        ev.filter = libc::EVFILT_TIMER;
        ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
        ev.data = ms.min(isize::MAX as u64) as isize;
        ev.udata = TIMER_UDATA as *mut libc::c_void;
        unsafe {
            libc::kevent(
                self.kq,
                &raw const ev,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            );
        }
    }

    fn unwatch_socket(&self, fd: libc::c_int) {
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = fd as usize;
        ev.filter = libc::EVFILT_READ;
        ev.flags = libc::EV_DELETE;
        unsafe {
            libc::kevent(
                self.kq,
                &raw const ev,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            );
        }
    }

    fn wait(&self) -> io::Result<Event> {
        loop {
            let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
            let n = unsafe {
                libc::kevent(
                    self.kq,
                    std::ptr::null(),
                    0,
                    &raw mut ev,
                    1,
                    std::ptr::null(),
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if ev.udata as usize == SOCKET_UDATA {
                return Ok(Event::Socket);
            }
            if ev.udata as usize == TIMER_UDATA {
                return Ok(Event::Timer);
            }
            if ev.fflags & libc::NOTE_EXIT != 0 {
                return Ok(Event::Exited {
                    index: ev.udata as usize,
                    status: ev.data as i32,
                });
            }
        }
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        unsafe { libc::close(self.kq) };
    }
}

/// The launcher's end of the run's socket
struct Channel {
    fd: libc::c_int,
    buf: Vec<u8>,
    /// Every guest has closed its end; a level-triggered read filter would
    /// fire forever
    closed: bool,
}

struct Frame {
    kind: u8,
    proc_index: u32,
    payload: Vec<u8>,
}

impl Channel {
    /// Returns the channel and the guests' end.
    fn pair() -> io::Result<(Channel, libc::c_int)> {
        let mut fds = [0 as libc::c_int; 2];
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        for fd in fds {
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        Ok((
            Channel {
                fd: fds[0],
                buf: Vec::new(),
                closed: false,
            },
            fds[1],
        ))
    }

    /// Complete frames that have arrived, without blocking.
    fn drain(&mut self) -> Vec<Frame> {
        let mut chunk = [0u8; 4096];
        loop {
            let n = unsafe {
                libc::recv(
                    self.fd,
                    chunk.as_mut_ptr().cast(),
                    chunk.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n == 0 {
                self.closed = true;
            }
            if n <= 0 {
                break;
            }
            self.buf.extend_from_slice(&chunk[..n as usize]);
        }
        let mut frames = Vec::new();
        while self.buf.len() >= 4 {
            let len = u32::from_le_bytes(self.buf[..4].try_into().unwrap()) as usize;
            if len < 5 || self.buf.len() < 4 + len {
                break;
            }
            let body: Vec<u8> = self.buf.drain(..4 + len).skip(4).collect();
            frames.push(Frame {
                kind: body[0],
                proc_index: u32::from_le_bytes(body[1..5].try_into().unwrap()),
                payload: body[5..].to_vec(),
            });
        }
        frames
    }

    fn reply(&self, errno: i32, payload: &[u8]) {
        let mut out = errno.to_le_bytes().to_vec();
        out.extend((payload.len() as u32).to_le_bytes());
        out.extend(payload);
        unsafe { libc::send(self.fd, out.as_ptr().cast(), out.len(), 0) };
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// Run every guest to completion under one scheduler. The launcher's
/// stdin/stdout/stderr are inherited unless a guest redirects stdout.
pub fn launch_run(run: &Run) -> io::Result<RunOutcome> {
    let coord = if run.dylib.is_some() && !run.passive {
        Some(Coordinator::create(run.seed, run.quantum)?)
    } else {
        None
    };
    let mut procs: Vec<Tracked> = Vec::new();
    let result = supervise(run, coord.as_ref(), &mut procs);
    if result.is_err() {
        kill_all(&mut procs);
    }
    let deadlock = result?;
    if let Some(coord) = &coord {
        coord.remove_ipc_objects();
    }
    let totals = coord.as_ref().map(Coordinator::totals).unwrap_or_default();
    let specs: Vec<Option<usize>> = procs.iter().map(|p| p.spec).collect();
    let wall_limited = procs.iter().any(|p| p.wall_limited);
    let guests = procs
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let mut report = Report::parse(&c.report);
            let stopped = totals.stopped.get(i).copied().unwrap_or(false) || c.wall_limited;
            if let Some((user, system)) = c.cpu {
                report.fields.insert("cpu_user_ns".into(), user.to_string());
                report
                    .fields
                    .insert("cpu_system_ns".into(), system.to_string());
            }
            if c.wall_limited {
                report.fields.insert("wall_limited".into(), "true".into());
            }
            // A guest the stop killed never reported; the run-wide values
            // are still worth having
            if coord.is_some() && (!report.fields.is_empty() || stopped) {
                // Run-wide values as of the end of the run, not of this
                // guest's exit
                for (k, v) in [
                    ("switches", totals.switches.to_string()),
                    ("expiries", totals.expiries.to_string()),
                    ("schedule_hash", format!("{:016x}", totals.schedule_hash)),
                ] {
                    report.fields.insert(k.into(), v);
                }
                if stopped {
                    report
                        .fields
                        .insert("stopped_at".into(), totals.stopped_at.to_string());
                }
            }
            Outcome {
                status: c.status.unwrap_or(0),
                report,
                image: c.image,
                program: c.program,
                stopped,
            }
        })
        .collect();
    Ok(RunOutcome {
        guests,
        wall_limited,
        initial: run.guests.len(),
        daemons: run.guests.iter().map(|g| g.daemon).collect(),
        specs,
        totals,
        deadlock,
    })
}

/// Only daemons (and what they spawned) are left: the run is over. Whoever
/// just exited held the baton, so everything else is parked and dies where
/// it stands, before the baton could be handed to it.
fn only_daemons_left(run: &Run, procs: &[Tracked]) -> bool {
    let work_left = procs
        .iter()
        .any(|p| p.status.is_none() && p.spec.is_some_and(|g| !run.guests[g].daemon));
    !work_left && run.guests.iter().any(|g| g.daemon)
}

/// The environment that makes a guest process `proc_index` of the run.
fn guest_env(run: &Run, coord: Option<(&Coordinator, u32)>) -> Vec<(String, String)> {
    let mut env = vec![("REWRITE_SEED".to_string(), run.seed.to_string())];
    // As wide whatever the size: the environment's length places the stack
    env.push((
        "REWRITE_HEAP".to_string(),
        format!("{:016x}", run.heap_size),
    ));
    if run.passive {
        env.push((PASSIVE_VAR.into(), "1".into()));
    }
    env.push((shared::EXTERNAL_VAR.into(), external_objects()));
    if let Some((coord, proc_index)) = coord {
        env.push((
            shared::SHARED_VAR.into(),
            coord.path().to_string_lossy().into_owned(),
        ));
        env.push((shared::PROC_VAR.into(), proc_index.to_string()));
    }
    env
}

/// Put `tracked` at `index`, which follows the shared process table and may
/// be ahead of what we have heard of.
fn track(procs: &mut Vec<Tracked>, index: usize, tracked: Tracked) {
    while procs.len() <= index {
        // A process whose spawn failed in the guest, or that we hear of later
        let mut t = Tracked::new(0, None);
        t.status = Some(0);
        procs.push(t);
    }
    procs[index] = tracked;
}

/// If the scheduler registered a next life for `index`, start it.
fn respawn(
    run: &Run,
    coord: &Coordinator,
    events: &Events,
    guest_sock: Option<libc::c_int>,
    procs: &mut Vec<Tracked>,
    gone: &mut Vec<usize>,
    index: usize,
) -> io::Result<()> {
    let (Some(new), Some(spec)) = (coord.replacement_of(index as u32), procs[index].spec) else {
        return Ok(());
    };
    let new = new as usize;
    if procs.get(new).is_some_and(|p| p.spec.is_some()) {
        return Ok(());
    }
    let env = guest_env(run, Some((coord, new as u32)));
    let pid = spawn(run, &run.guests[spec], &env, guest_sock, true)?;
    track(procs, new, Tracked::new(pid, Some(spec)));
    if !events.watch_exit(pid, new) {
        procs[new].reap(true);
        gone.push(new);
    }
    Ok(())
}

/// Only daemons are left. With a scheduler they are stopped at the virtual
/// time reached, like `stop-after`, so that they report and exit on their
/// own; a daemon that never takes the baton up again is killed after a
/// real-time grace period. Without one they are killed now.
fn stop_or_end(
    coord: Option<&Coordinator>,
    events: &Events,
    procs: &mut [Tracked],
    stopping: &mut bool,
) {
    match coord {
        Some(coord) if !*stopping => {
            coord.stop_now();
            events.watch_timer(STOP_GRACE_MS);
            *stopping = true;
        }
        Some(_) => {}
        None => end_run(procs),
    }
}

const STOP_GRACE_MS: u64 = 5000;

fn end_run(procs: &mut [Tracked]) {
    kill_all(procs);
    for p in procs.iter_mut() {
        p.status.get_or_insert(libc::SIGKILL);
    }
}

fn kill_all(procs: &mut [Tracked]) {
    for c in procs.iter_mut() {
        c.kill();
        if c.ours() {
            c.reap(true);
        }
    }
}

/// `dev:ino` of the pipes and sockets among our own standard descriptors.
/// Their other end is outside the run, so guests must block on them for
/// real instead of waiting for another guest to make them ready.
///
/// Always three entries of the same width, whatever our descriptors are:
/// the variable's length must not differ between runs (see the shared file's
/// name in `coord.rs`). An entry of zeros matches nothing.
fn external_objects() -> String {
    let mut out = Vec::new();
    for fd in 0..3 {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let known = unsafe { libc::fstat(fd, &raw mut st) } == 0;
        let kind = st.st_mode & libc::S_IFMT;
        if known && (kind == libc::S_IFIFO || kind == libc::S_IFSOCK) {
            out.push(format!("{:011}:{:020}", st.st_dev, st.st_ino));
        } else {
            out.push(format!("{:011}:{:020}", 0, 0));
        }
    }
    out.join(",")
}

/// A guest asks where the rewritten form of a program it wants to run is.
fn rewritten_path(run: &Run, payload: &[u8]) -> Result<Vec<u8>, i32> {
    let path = Path::new(std::ffi::OsStr::from_bytes(payload));
    let Some(opts) = &run.rewrite else {
        return Ok(payload.to_vec());
    };
    match crate::cache::cached_rewrite(path, opts) {
        Ok(p) => Ok(p.as_os_str().as_bytes().to_vec()),
        Err(e) => {
            eprintln!(
                "rewrite: cannot rewrite {} for a guest: {e}",
                path.display()
            );
            Err(libc::ENOEXEC)
        }
    }
}

/// Returns whether the run ended in a deadlock.
fn supervise(run: &Run, coord: Option<&Coordinator>, procs: &mut Vec<Tracked>) -> io::Result<bool> {
    let events = Events::new()?;
    if run.wall_limit_ms != 0 {
        events.watch_timer(run.wall_limit_ms);
    }
    let mut channel = None;
    let mut guest_sock = None;
    if coord.is_some() {
        let (c, g) = Channel::pair()?;
        events.watch_socket(c.fd);
        channel = Some(c);
        guest_sock = Some(g);
    }
    if let Some(c) = coord {
        c.set_net_latency(run.net_latency_ns);
        c.set_switch_cost(run.switch_ns);
        c.set_outside_network(run.outside_network);
        c.set_stop_at(run.stop_at_ns);
        c.set_debug_paths();
        if let Some((at, with)) = run.reseed {
            c.set_reseed(at, with);
        }
        c.add_hosts(&run.hosts);
    }
    for (spec, guest) in run.guests.iter().enumerate() {
        let registered = coord.map(|c| (c, c.register(guest.host, guest.faults, spec as u32)));
        debug_assert!(registered.is_none_or(|(_, pid)| pid as usize == procs.len()));
        let spawned = spawn(run, guest, &guest_env(run, registered), guest_sock, false);
        if spawned.is_err() {
            if let Some(g) = guest_sock {
                unsafe { libc::close(g) };
            }
        }
        procs.push(Tracked::new(spawned?, Some(spec)));
    }
    // The guests' end of the socket stays open here: a restarted process
    // inherits it from us.
    let result = supervise_started(run, coord, &events, channel.as_mut(), guest_sock, procs);
    if let Some(g) = guest_sock {
        unsafe { libc::close(g) };
    }
    result
}

fn supervise_started(
    run: &Run,
    coord: Option<&Coordinator>,
    events: &Events,
    mut channel: Option<&mut Channel>,
    guest_sock: Option<libc::c_int>,
    procs: &mut Vec<Tracked>,
) -> io::Result<bool> {
    if let Some(coord) = coord {
        coord.wait_attached(procs.len() as u32, |pid| procs[pid as usize].reap(false))?;
    }
    // Guests that died before attaching were reaped above; the rest are
    // watched. A failed watch means the process went in between.
    let mut gone: Vec<usize> = Vec::new();
    for (i, p) in procs.iter_mut().enumerate() {
        if p.status.is_some() || !events.watch_exit(p.pid, i) {
            p.reap(true);
            gone.push(i);
        }
    }
    if let Some(coord) = coord {
        // A replacement may be gone before it is watched, too
        while let Some(i) = gone.pop() {
            coord.process_died(i as u32, procs[i].status.unwrap_or(0));
            respawn(run, coord, events, guest_sock, procs, &mut gone, i)?;
        }
        coord.start();
    }
    let mut stopping = false;
    if only_daemons_left(run, procs) {
        stop_or_end(coord, events, procs, &mut stopping);
    }

    let mut deadlock = false;
    while procs.iter().any(|c| c.status.is_none()) {
        let event = events.wait()?;
        // A dying guest's last frames may still be queued behind its exit
        if let Some(ch) = channel.as_deref_mut() {
            for frame in ch.drain() {
                handle_frame(run, ch, events, procs, &mut gone, &frame);
            }
            if std::mem::take(&mut ch.closed) {
                events.unwatch_socket(ch.fd);
            }
        }
        if let Event::Timer = event {
            if !stopping {
                for p in procs.iter_mut() {
                    p.wall_limited = p.status.is_none();
                }
            }
            end_run(procs);
            break;
        }
        if let Event::Exited { index, status } = event {
            gone.push(index);
            // `waitpid` fails if SIGCHLD is ignored in our own environment;
            // the event carries the status too
            let p = &mut procs[index];
            if !(p.ours() && p.reap(true)) {
                p.status = Some(status);
            }
        }
        while let Some(index) = gone.pop() {
            let status = procs[index].status.unwrap_or(0);
            let restarting = coord.is_some_and(|c| c.will_restart(index as u32, status));
            let last = !restarting && only_daemons_left(run, procs);
            if last {
                if let Some(coord) = coord {
                    coord.note_last_death(index as u32);
                }
                // Armed before the death is passed on below: the hand-off
                // then finds the stop among its deadlines
                stop_or_end(coord, events, procs, &mut stopping);
                if !stopping {
                    break;
                }
            }
            let Some(coord) = coord else { continue };
            if coord.process_died(index as u32, status) {
                deadlock = true;
                end_run(procs);
                break;
            }
            respawn(run, coord, events, guest_sock, procs, &mut gone, index)?;
        }
    }
    Ok(deadlock)
}

fn handle_frame(
    run: &Run,
    channel: &Channel,
    events: &Events,
    procs: &mut Vec<Tracked>,
    gone: &mut Vec<usize>,
    frame: &Frame,
) {
    match frame.kind {
        shared::MSG_SPAWN => match rewritten_path(run, &frame.payload) {
            Ok(path) => channel.reply(0, &path),
            Err(errno) => channel.reply(errno, &[]),
        },
        shared::MSG_SPAWNED if frame.payload.len() == 8 => {
            let child = u32::from_le_bytes(frame.payload[..4].try_into().unwrap()) as usize;
            let pid = i32::from_le_bytes(frame.payload[4..].try_into().unwrap());
            // A fork runs the parent's program; a spawn says its own soon
            let (image, program) = procs
                .get(frame.proc_index as usize)
                .map_or((None, None), |p| (p.image.clone(), p.program.clone()));
            let mut tracked = Tracked::new(pid, None);
            (tracked.image, tracked.program) = (image, program);
            track(procs, child, tracked);
            if !events.watch_exit(pid, child) {
                procs[child].status = Some(0);
                gone.push(child);
            }
            channel.reply(0, &[]);
        }
        shared::MSG_REPORT => {
            if let Some(p) = procs.get_mut(frame.proc_index as usize) {
                p.report = String::from_utf8_lossy(&frame.payload).into_owned();
            }
        }
        shared::MSG_IMAGE => {
            if let Some(p) = procs.get_mut(frame.proc_index as usize) {
                let image = PathBuf::from(std::ffi::OsStr::from_bytes(&frame.payload));
                p.program =
                    Some(crate::cache::original_of(&image).unwrap_or_else(|| image.clone()));
                p.image = Some(image);
            }
            channel.reply(0, &[]);
        }
        other => eprintln!("rewrite: unknown frame type {other} from a guest"),
    }
}

/// Run one guest to completion.
pub fn launch(cfg: &Launch) -> io::Result<Outcome> {
    let run = Run {
        guests: vec![Guest {
            exe: cfg.exe.clone(),
            argv0: None,
            args: cfg.args.clone(),
            host: 0,
            env: Vec::new(),
            stdout: cfg.stdout.clone(),
            stderr: cfg.stderr.clone(),
            cwd: None,
            daemon: false,
            faults: shared::Faults::default(),
        }],
        hosts: vec!["h0".into()],
        dylib: cfg.dylib.clone(),
        inherit_env: true,
        disable_aslr: cfg.disable_aslr,
        heap_size: cfg.heap_size,
        seed: cfg.seed,
        quantum: cfg.quantum,
        passive: cfg.passive,
        rewrite: cfg.rewrite.clone(),
        net_latency_ns: 0,
        switch_ns: crate::shared::DEFAULT_SWITCH_NS,
        reseed: None,
        stop_at_ns: cfg.stop_at_ns,
        wall_limit_ms: cfg.wall_limit_ms,
        // A single program inherits our environment anyway
        outside_network: true,
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
