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
const POSIX_SPAWN_SETEXEC: libc::c_short = 0x0040;

/// What a child needs in its environment to join the run, captured at
/// startup because the guest may rewrite or clear its own environment.
struct Inherit {
    dylib: String,
    shared: String,
    seed: String,
    external: String,
    host_root: String,
    allow: String,
}

static INHERIT: SpinLock<Option<Inherit>> = SpinLock::new(None);

const OUR_VARS: [&str; 7] = [
    shared::HOST_ROOT_VAR,
    shared::ALLOW_VAR,
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
        host_root: get(shared::HOST_ROOT_VAR),
        allow: get(shared::ALLOW_VAR),
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

/// The guest's environment without our variables.
fn guest_env(envp: *const *mut c_char) -> Vec<CString> {
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
    out
}

/// The guest's environment for a child of the run, with our variables
/// set for process `proc_index`.
fn child_env(envp: *const *mut c_char, proc_index: u32) -> Vec<CString> {
    let mut out = guest_env(envp);
    if let Some(i) = INHERIT.lock().as_ref() {
        for (k, v) in [
            ("DYLD_INSERT_LIBRARIES", i.dylib.as_str()),
            (shared::SHARED_VAR, i.shared.as_str()),
            ("REWRITE_SEED", i.seed.as_str()),
            (shared::EXTERNAL_VAR, i.external.as_str()),
            (shared::HOST_ROOT_VAR, i.host_root.as_str()),
            (shared::ALLOW_VAR, i.allow.as_str()),
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
    // Program images live outside every host's directory
    let resolved = crate::hostfs::exempt(|| unsafe { libc::realpath(path, buf.as_mut_ptr()) });
    if resolved.is_null() {
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
        let found = crate::hostfs::exempt(|| unsafe { libc::access(c.as_ptr(), libc::X_OK) });
        (found == 0).then_some(c)
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

/// Wait, in real time, until the new process has attached (or died
/// trying). It counts the sockets it inherited before it goes live; until
/// then a close of ours could make a shared socket look unused. The
/// outcome does not depend on how long this takes.
fn await_attach(child: u32, real: libc::pid_t) {
    loop {
        let starting =
            sched::with(|s, _| s.procs[child as usize].state == shared::P_STARTING) == Some(true);
        if !starting {
            return;
        }
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                real as libc::id_t,
                &raw mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc != 0 || info.si_pid == real {
            return;
        }
        unsafe { libc::usleep(100) };
    }
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

/// A spawn the scheduler has no part in (from a GCD worker, say). The child
/// must not inherit our variables: with `REWRITE_PROC` it would attach as
/// its parent's process.
unsafe fn spawn_outside_the_run(
    pid_out: *mut libc::pid_t,
    path: *const c_char,
    actions: *const libc::posix_spawn_file_actions_t,
    attr: *const libc::posix_spawnattr_t,
    argv: *const *mut c_char,
    envp: *const *mut c_char,
) -> c_int {
    if !in_run() {
        return libc::posix_spawn(pid_out, path, actions, attr, argv, envp);
    }
    let env = guest_env(envp);
    let env_ptrs = pointers(&env);
    libc::posix_spawn(pid_out, path, actions, attr, argv, env_ptrs.as_ptr())
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
    // With POSIX_SPAWN_SETEXEC this is an exec: the new image replaces this
    // process (Python's launcher stub becomes the real interpreter so).
    let mut flags: libc::c_short = 0;
    if !attr.is_null() {
        libc::posix_spawnattr_getflags(attr, &raw mut flags);
    }
    if flags & POSIX_SPAWN_SETEXEC != 0 {
        let env = child_env(envp, sched::pid());
        let env_ptrs = pointers(&env);
        let retired = retire_other_threads();
        let rc = libc::posix_spawn(
            pid_out,
            exe.as_ptr(),
            actions,
            attr,
            argv,
            env_ptrs.as_ptr(),
        );
        restore_threads(&retired);
        return rc;
    }
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
    // The child writes this too, but one that dies before attaching never
    // does, and a real pid of 0 would turn `wait4` and `kill` loose on the
    // whole process group.
    sched::with(|s, _| s.procs[child as usize].real_pid = real);
    crate::coord::spawned(child, real);
    await_attach(child, real);
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
        return spawn_outside_the_run(pid_out, path, actions, attr, argv, envp);
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
        return crate::errno::fail(e);
    }
    sched::with(|s, _| s.procs[child as usize].real_pid = real);
    crate::coord::spawned(child, real);
    await_attach(child, real);
    vpid_of(child)
}

/// Before an exec: the other threads of this process die with the old
/// image. Returns what to put back if the exec fails.
fn retire_other_threads() -> Vec<(usize, u32)> {
    let me = my_id().unwrap();
    sched::with(|s, pid| {
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
    .unwrap_or_default()
}

fn restore_threads(retired: &[(usize, u32)]) {
    sched::with(|s, _| {
        for &(i, state) in retired {
            s.threads[i].state = state;
        }
    });
}

/// On arm64 Darwin `vfork` is `fork`.
pub unsafe extern "C" fn my_vfork() -> libc::pid_t {
    my_fork()
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
        Err(e) => return crate::errno::fail(e),
    };
    // Same process of the run, new image: the new supervisor picks up this
    // thread's record, still holding the baton. The other threads die with
    // the old image.
    let env = child_env(envp, sched::pid());
    let env_ptrs = pointers(&env);
    let retired = retire_other_threads();
    libc::execve(exe.as_ptr(), argv.cast(), env_ptrs.as_ptr().cast());
    let e = *libc::__error();
    restore_threads(&retired);
    crate::errno::fail(e)
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
    // A real pid is a child the scheduler does not know (a system library
    // waiting for a helper it started): the real call is the right one.
    if vpid > 0 && !shared::is_virtual_pid(vpid) {
        return libc::wait4(vpid, status, options, rusage);
    }
    let which = if vpid > 0 {
        match proc_of(vpid) {
            Some(p) => Some(p),
            None => return crate::errno::fail(libc::ECHILD),
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
            Waited::NoChild => return crate::errno::fail(libc::ECHILD),
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

/// Kernel thread ids are system-wide and differ from run to run; `RocksDB`
/// mixes one into every DB session id. A scheduled thread's is its index in
/// the run. Threads outside the schedule keep the kernel's.
const VTID_BASE: u64 = 1_000_000_000;

pub unsafe extern "C" fn my_pthread_threadid_np(thread: libc::pthread_t, tid: *mut u64) -> c_int {
    if in_run() && !tid.is_null() {
        let id = if thread == 0 {
            sched::my_id()
        } else {
            sched::with(|s, pid| s.find_pthread(pid, thread as u64)).flatten()
        };
        if let Some(id) = id {
            *tid = VTID_BASE + id as u64;
            return 0;
        }
    }
    libc::pthread_threadid_np(thread, tid)
}

extern "C" {
    fn _dyld_get_shared_cache_range(length: *mut usize) -> *const std::ffi::c_void;
}

static RANGE: SpinLock<Option<(usize, usize)>> = SpinLock::new(None);
static ALLOCATOR: SpinLock<Option<(usize, usize)>> = SpinLock::new(None);

/// In the child of a `fork`: see `SpinLock::force_unlock`.
pub fn forked() {
    RANGE.force_unlock();
    ALLOCATOR.force_unlock();
    INHERIT.force_unlock();
}

/// Whether `address` is code of a system library. Virtual pids are for the
/// guest's own code and the libraries it brought. libSystem's internals
/// hand `getpid()` to the kernel (unified logging asks `proc_pidinfo` about
/// it while CoreFoundation initializes, and crashes on an error), so they
/// must see the real one.
pub fn in_system_library(address: usize) -> bool {
    let mut range = RANGE.lock();
    let (start, len) = *range.get_or_insert_with(|| {
        let mut len = 0usize;
        let start = unsafe { _dyld_get_shared_cache_range(&raw mut len) };
        (start as usize, len)
    });
    address >= start && address - start < len
}

extern "C" {
    fn _dyld_image_count() -> u32;
    fn _dyld_get_image_name(i: u32) -> *const std::ffi::c_char;
    fn _dyld_get_image_header(index: u32) -> *const u32;
    fn getsegmentdata(
        mhp: *const u8,
        segname: *const std::ffi::c_char,
        size: *mut libc::c_ulong,
    ) -> *mut u8;
}

/// Whether `address` is code of the system allocator, `libsystem_malloc`.
pub fn in_system_allocator(address: usize) -> bool {
    if !in_system_library(address) {
        return false;
    }
    let mut range = ALLOCATOR.lock();
    let (start, len) = *range.get_or_insert_with(|| unsafe {
        for i in 0.._dyld_image_count() {
            let name = std::ffi::CStr::from_ptr(_dyld_get_image_name(i)).to_bytes();
            if name.ends_with(b"/libsystem_malloc.dylib") {
                let mut size: libc::c_ulong = 0;
                let text = getsegmentdata(
                    _dyld_get_image_header(i).cast(),
                    c"__TEXT".as_ptr(),
                    &raw mut size,
                );
                return (text as usize, size as usize);
            }
        }
        (0, 0)
    });
    address >= start && address - start < len
}

/// `caller` is the return address of the `getpid` call, from the shim.
#[no_mangle]
pub extern "C" fn rewrite_getpid_impl(caller: usize) -> libc::pid_t {
    if !in_run() || in_system_library(caller) {
        return unsafe { libc::getpid() };
    }
    vpid_of(sched::pid())
}

#[no_mangle]
pub extern "C" fn rewrite_getppid_impl(caller: usize) -> libc::pid_t {
    if !in_run() || in_system_library(caller) {
        return unsafe { libc::getppid() };
    }
    match sched::with(|s, me| s.procs[me as usize].parent) {
        Some(parent) if parent != shared::NO_PROC => vpid_of(parent),
        _ => 1,
    }
}

extern "C" {
    pub fn rewrite_getpid_shim();
    pub fn rewrite_getppid_shim();
}

// The link register is the caller's return address; the tail call keeps it.
std::arch::global_asm!(
    ".globl _rewrite_getpid_shim",
    ".p2align 2",
    "_rewrite_getpid_shim:",
    "mov x0, x30",
    "b _rewrite_getpid_impl",
    ".globl _rewrite_getppid_shim",
    ".p2align 2",
    "_rewrite_getppid_shim:",
    "mov x0, x30",
    "b _rewrite_getppid_impl",
);

/// `pthread_kill` to another scheduled thread of this process (Go
/// preempts goroutines and stops the world this way, with SIGURG): the
/// target is parked, so the signal is pending against it and raised when
/// it next takes the baton up, where its handler runs with the baton. To
/// the calling thread, or to a thread the scheduler does not run, it is
/// the kernel's.
pub unsafe extern "C" fn my_pthread_kill(t: libc::pthread_t, sig: c_int) -> c_int {
    if sig == 0 || !(1..32).contains(&sig) || sched::my_id().is_none() || t == libc::pthread_self()
    {
        return libc::pthread_kill(t, sig);
    }
    let queued = sched::with(|s, pid| {
        let id = s.find_pthread(pid, t as u64)?;
        if s.threads[id].state == shared::T_EXITED {
            return None;
        }
        s.threads[id].sig_pending |= 1 << sig;
        Some(())
    })
    .flatten();
    match queued {
        Some(()) => 0,
        None => libc::pthread_kill(t, sig),
    }
}

pub unsafe extern "C" fn my_kill(vpid: libc::pid_t, sig: c_int) -> c_int {
    if !in_run() || !shared::is_virtual_pid(vpid) {
        return libc::kill(vpid, sig);
    }
    let Some(target) = proc_of(vpid) else {
        return crate::errno::fail(libc::ESRCH);
    };
    let known = sched::with(|s, _| {
        let p = &s.procs[target as usize];
        (target < s.nprocs && p.state != shared::P_EXITED && !p.killed).then_some(p.real_pid)
    })
    .flatten();
    let Some(real) = known else {
        return crate::errno::fail(libc::ESRCH);
    };
    if target == sched::pid() {
        // To this thread, not to whichever thread the kernel would pick at
        // whatever moment: the handler then runs here and now, with the
        // baton, as a point of the schedule.
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &raw mut mask);
        // Blocked here, the kernel must find a thread that takes it
        let taken_here = sig != 0 && libc::sigismember(&raw const mask, sig) == 0;
        if (1..32).contains(&sig) {
            sched::with(|s, pid| s.procs[pid as usize].sig_counts[sig as usize] += 1);
        }
        if taken_here && sched::on_scheduled_thread() {
            let rc = libc::pthread_kill(libc::pthread_self(), sig);
            return if rc == 0 { 0 } else { crate::errno::fail(rc) };
        }
        return libc::kill(real, sig);
    }
    if sig == 0 {
        return 0;
    }
    if sig != libc::SIGTERM && sig != libc::SIGKILL {
        if !(1..32).contains(&sig) {
            return crate::errno::fail(libc::EINVAL);
        }
        // The target is parked: it takes the signal when it next takes
        // the baton up, and a wait of its on the signal sees it now
        sched::with(|s, _| {
            let p = &mut s.procs[target as usize];
            p.sig_pending |= 1 << sig;
            p.sig_counts[sig as usize] += 1;
        });
        sched::wake_io();
        return 0;
    }
    if sig == libc::SIGTERM {
        // The target is parked. A SIGTERM handler would run its code without
        // the baton, and a process that survives could never be scheduled
        // again, so the signal that arrives is the one that cannot be caught.
        static NOTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !NOTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            crate::report::log("kill: SIGTERM to another guest is delivered as SIGKILL");
        }
    }
    // The target is parked, so it dies where it stands. All that its death
    // means to the run happens now, under the lock: the launcher only hears
    // of it later, and the baton must not go to a thread that is gone.
    // One still down between lives has no real process yet.
    sched::with(|s, _| {
        s.crash(target);
        sched::signal_crashed(s);
    });
    sched::settle_deaths();
    0
}
