//! Process lifecycle under the run's scheduler. A guest that starts
//! another process registers it in the shared state itself (it holds the
//! baton), so process and thread ids follow spawn order; the child starts
//! parked and runnable. The launcher is asked for the rewritten binary and
//! told the child's real pid so it can watch for its death, which is what
//! wakes a parent blocked in `waitpid`.
//!
//! Guests see virtual pids (`shared::vpid_of`), so pids in output and in
//! temp-file names repeat from run to run.

use std::ffi::{c_char, c_int, CStr, CString};

use crate::sched::{self, my_id, State};
use crate::shared::{self, proc_of, vpid_of};
use crate::spin::SpinLock;

/// Not in the libc crate; from `<spawn.h>` on Darwin
const POSIX_SPAWN_DISABLE_ASLR: libc::c_short = 0x0100;

/// What a child needs in its environment to join the run, captured at
/// startup because the guest may rewrite or clear its own environment.
struct Inherit {
    dylib: String,
    shared: String,
    seed: String,
    external: String,
}

static INHERIT: SpinLock<Option<Inherit>> = SpinLock::new(None);

const OUR_VARS: [&str; 5] = [
    "DYLD_INSERT_LIBRARIES",
    shared::SHARED_VAR,
    shared::PROC_VAR,
    shared::EXTERNAL_VAR,
    "REWRITE_SEED",
];

pub fn init() {
    let get = |k: &str| std::env::var(k).unwrap_or_default();
    *INHERIT.lock() = Some(Inherit {
        dylib: get("DYLD_INSERT_LIBRARIES"),
        shared: get(shared::SHARED_VAR),
        seed: get("REWRITE_SEED"),
        external: get(shared::EXTERNAL_VAR),
    });
}

/// Whether process calls on this thread are ours to handle: a registered
/// thread in a run that has a launcher.
fn managed() -> bool {
    my_id().is_some() && crate::coord::connected()
}

extern "C" {
    static environ: *const *mut c_char;
}

fn set_errno(e: c_int) -> c_int {
    unsafe { *libc::__error() = e };
    -1
}

/// The guest's environment for a child, with our variables replaced.
fn child_env(envp: *const *mut c_char, proc_index: u32) -> Vec<CString> {
    let mut out = Vec::new();
    let mut p = if envp.is_null() {
        unsafe { environ }
    } else {
        envp
    };
    while !p.is_null() && !unsafe { *p }.is_null() {
        let entry = unsafe { CStr::from_ptr(*p) };
        let bytes = entry.to_bytes();
        let ours = OUR_VARS
            .iter()
            .any(|k| bytes.starts_with(k.as_bytes()) && bytes.get(k.len()) == Some(&b'='));
        if !ours {
            out.push(entry.to_owned());
        }
        p = unsafe { p.add(1) };
    }
    if let Some(i) = INHERIT.lock().as_ref() {
        for (k, v) in [
            ("DYLD_INSERT_LIBRARIES", i.dylib.as_str()),
            (shared::SHARED_VAR, i.shared.as_str()),
            ("REWRITE_SEED", i.seed.as_str()),
            (shared::EXTERNAL_VAR, i.external.as_str()),
        ] {
            out.push(CString::new(format!("{k}={v}")).unwrap());
        }
    }
    out.push(CString::new(format!("{}={proc_index}", shared::PROC_VAR)).unwrap());
    out
}

fn pointers(v: &[CString]) -> Vec<*mut c_char> {
    let mut p: Vec<*mut c_char> = v.iter().map(|s| s.as_ptr().cast_mut()).collect();
    p.push(std::ptr::null_mut());
    p
}

/// Absolute path of the rewritten form of `path`, or an errno.
fn rewritten(path: *const c_char) -> Result<CString, c_int> {
    let mut buf = [0 as c_char; libc::PATH_MAX as usize];
    if unsafe { libc::realpath(path, buf.as_mut_ptr()) }.is_null() {
        return Err(unsafe { *libc::__error() });
    }
    let abs = unsafe { CStr::from_ptr(buf.as_ptr()) };
    let out = crate::coord::rewritten(abs.to_bytes())?;
    CString::new(out).map_err(|_| libc::EINVAL)
}

/// Search `PATH` the way `posix_spawnp` does.
fn search_path(file: &CStr) -> Option<CString> {
    if file.to_bytes().contains(&b'/') {
        return Some(file.to_owned());
    }
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    path.split(':').find_map(|dir| {
        let dir = if dir.is_empty() { "." } else { dir };
        let mut full = dir.as_bytes().to_vec();
        full.push(b'/');
        full.extend(file.to_bytes());
        let c = CString::new(full).ok()?;
        (unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0).then_some(c)
    })
}

/// Register a child of this process on the same host, with its main
/// thread runnable.
fn register_child() -> u32 {
    sched::with(|s, me| {
        let host = s.procs[me as usize].host;
        let child = s.add_proc(host, me);
        s.add_thread(child);
        child
    })
    .expect("scheduler not initialized")
}

/// The child never came to be: nobody will run or reap it.
fn unregister_child(child: u32) {
    sched::with(|s, _| {
        let p = &mut s.procs[child as usize];
        p.state = shared::P_EXITED;
        p.reaped = true;
        let n = s.nthreads as usize;
        for t in &mut s.threads[..n] {
            if t.pid == child {
                t.state = shared::T_EXITED;
            }
        }
    });
}

unsafe fn spawn_rewritten(
    pid_out: *mut libc::pid_t,
    path: *const c_char,
    actions: *const libc::posix_spawn_file_actions_t,
    attr: *const libc::posix_spawnattr_t,
    argv: *const *mut c_char,
    envp: *const *mut c_char,
) -> c_int {
    let exe = match rewritten(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let child = register_child();
    let env = child_env(envp, child);
    let env_ptrs = pointers(&env);

    // The child must load where every other guest does.
    let mut own_attr: libc::posix_spawnattr_t = std::mem::zeroed();
    let mut saved_flags: libc::c_short = 0;
    let attr_mut = if attr.is_null() {
        libc::posix_spawnattr_init(&raw mut own_attr);
        &raw mut own_attr
    } else {
        libc::posix_spawnattr_getflags(attr, &raw mut saved_flags);
        attr.cast_mut()
    };
    libc::posix_spawnattr_setflags(attr_mut, saved_flags | POSIX_SPAWN_DISABLE_ASLR);

    let mut real: libc::pid_t = 0;
    let rc = libc::posix_spawn(
        &raw mut real,
        exe.as_ptr(),
        actions,
        attr_mut,
        argv,
        env_ptrs.as_ptr(),
    );
    if attr.is_null() {
        libc::posix_spawnattr_destroy(&raw mut own_attr);
    } else {
        libc::posix_spawnattr_setflags(attr_mut, saved_flags);
    }
    if rc != 0 {
        unregister_child(child);
        return rc;
    }
    crate::coord::spawned(child, real);
    if !pid_out.is_null() {
        *pid_out = vpid_of(child);
    }
    0
}

pub unsafe extern "C" fn my_posix_spawn(
    pid_out: *mut libc::pid_t,
    path: *const c_char,
    actions: *const libc::posix_spawn_file_actions_t,
    attr: *const libc::posix_spawnattr_t,
    argv: *const *mut c_char,
    envp: *const *mut c_char,
) -> c_int {
    if !managed() {
        return libc::posix_spawn(pid_out, path, actions, attr, argv, envp);
    }
    spawn_rewritten(pid_out, path, actions, attr, argv, envp)
}

pub unsafe extern "C" fn my_posix_spawnp(
    pid_out: *mut libc::pid_t,
    file: *const c_char,
    actions: *const libc::posix_spawn_file_actions_t,
    attr: *const libc::posix_spawnattr_t,
    argv: *const *mut c_char,
    envp: *const *mut c_char,
) -> c_int {
    if !managed() {
        return libc::posix_spawnp(pid_out, file, actions, attr, argv, envp);
    }
    match search_path(CStr::from_ptr(file)) {
        Some(path) => spawn_rewritten(pid_out, path.as_ptr(), actions, attr, argv, envp),
        None => libc::ENOENT,
    }
}

pub unsafe extern "C" fn my_fork() -> libc::pid_t {
    if !managed() {
        return libc::fork();
    }
    let child = register_child();
    let real = libc::fork();
    if real == 0 {
        // Only the calling thread exists here. The child starts parked
        // like a spawned one; the parent keeps the baton.
        sched::become_forked_child(child);
        return 0;
    }
    if real < 0 {
        let e = *libc::__error();
        unregister_child(child);
        return set_errno(e);
    }
    crate::coord::spawned(child, real);
    vpid_of(child)
}

pub unsafe extern "C" fn my_execve(
    path: *const c_char,
    argv: *const *mut c_char,
    envp: *const *mut c_char,
) -> c_int {
    if !managed() {
        return libc::execve(path, argv.cast(), envp.cast());
    }
    let exe = match rewritten(path) {
        Ok(p) => p,
        Err(e) => return set_errno(e),
    };
    // Same process of the run, new image: the new supervisor picks up this
    // thread's record, still holding the baton. The other threads die with
    // the old image.
    let me = my_id().unwrap();
    let env = child_env(envp, sched::pid());
    let env_ptrs = pointers(&env);
    let retired = sched::with(|s, pid| {
        let n = s.nthreads as usize;
        let mut retired = Vec::new();
        for (i, t) in s.threads[..n].iter_mut().enumerate() {
            if t.pid == pid && i != me && t.state != shared::T_EXITED {
                retired.push((i, t.state));
                t.state = shared::T_EXITED;
            }
        }
        retired
    })
    .unwrap_or_default();
    libc::execve(exe.as_ptr(), argv.cast(), env_ptrs.as_ptr().cast());
    let e = *libc::__error();
    sched::with(|s, _| {
        for &(i, state) in &retired {
            s.threads[i].state = state;
        }
    });
    set_errno(e)
}

enum Waited {
    Found(u32, libc::pid_t),
    NoChild,
    NotYet,
}

unsafe fn wait_for_child(
    vpid: libc::pid_t,
    status: *mut c_int,
    options: c_int,
    rusage: *mut libc::rusage,
) -> libc::pid_t {
    // Process groups are not modelled: any negative pid or 0 means any child
    let which = if vpid > 0 {
        match proc_of(vpid) {
            Some(p) => Some(p),
            None => return set_errno(libc::ECHILD),
        }
    } else {
        None
    };
    loop {
        let found = sched::with(|s, me| {
            if let Some(c) = s.exited_child(me, which) {
                s.procs[c as usize].reaped = true;
                Waited::Found(c, s.procs[c as usize].real_pid)
            } else if s.has_child(me, which) {
                Waited::NotYet
            } else {
                Waited::NoChild
            }
        })
        .unwrap_or(Waited::NoChild);
        match found {
            // The launcher saw the exit, so this returns without a real wait
            Waited::Found(child, real) => {
                let rc = libc::wait4(real, status, options & !libc::WNOHANG, rusage);
                return if rc < 0 { rc } else { vpid_of(child) };
            }
            Waited::NoChild => return set_errno(libc::ECHILD),
            Waited::NotYet if options & libc::WNOHANG != 0 => return 0,
            Waited::NotYet => {
                sched::yield_baton(State::Blocked(shared::WAIT_KEY as usize), shared::WAIT_KEY);
            }
        }
    }
}

pub unsafe extern "C" fn my_waitpid(
    pid: libc::pid_t,
    status: *mut c_int,
    options: c_int,
) -> libc::pid_t {
    if !managed() {
        return libc::waitpid(pid, status, options);
    }
    wait_for_child(pid, status, options, std::ptr::null_mut())
}

pub unsafe extern "C" fn my_wait4(
    pid: libc::pid_t,
    status: *mut c_int,
    options: c_int,
    rusage: *mut libc::rusage,
) -> libc::pid_t {
    if !managed() {
        return libc::wait4(pid, status, options, rusage);
    }
    wait_for_child(pid, status, options, rusage)
}

pub unsafe extern "C" fn my_wait(status: *mut c_int) -> libc::pid_t {
    if !managed() {
        return libc::wait(status);
    }
    wait_for_child(-1, status, 0, std::ptr::null_mut())
}

/// In a launcher's run, even for threads the scheduler does not know
fn in_run() -> bool {
    crate::coord::connected()
}

pub extern "C" fn my_getpid() -> libc::pid_t {
    if !in_run() {
        return unsafe { libc::getpid() };
    }
    vpid_of(sched::pid())
}

pub extern "C" fn my_getppid() -> libc::pid_t {
    if !in_run() {
        return unsafe { libc::getppid() };
    }
    match sched::with(|s, me| s.procs[me as usize].parent) {
        Some(parent) if parent != shared::NO_PROC => vpid_of(parent),
        _ => 1,
    }
}

pub unsafe extern "C" fn my_kill(vpid: libc::pid_t, sig: c_int) -> c_int {
    let target = if in_run() { proc_of(vpid) } else { None };
    let Some(target) = target else {
        return libc::kill(vpid, sig);
    };
    let known = sched::with(|s, _| {
        let p = &s.procs[target as usize];
        (target < s.nprocs && p.state != shared::P_EXITED).then_some(p.real_pid)
    })
    .flatten();
    let Some(real) = known else {
        return set_errno(libc::ESRCH);
    };
    if target == sched::pid() || sig == 0 {
        return libc::kill(real, sig);
    }
    if sig != libc::SIGTERM && sig != libc::SIGKILL {
        crate::report::log("kill: only SIGTERM and SIGKILL reach another guest; signal dropped");
        return 0;
    }
    // The target is parked, so it dies where it stands. Its threads must
    // leave the runnable set now: the launcher only hears of the death
    // later, and the baton must not go to a thread that no longer exists.
    sched::with(|s, _| {
        let n = s.nthreads as usize;
        for t in &mut s.threads[..n] {
            if t.pid == target {
                t.state = shared::T_EXITED;
                t.cond_key = 0;
            }
        }
    });
    libc::kill(real, sig)
}
