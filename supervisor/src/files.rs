//! Files are not virtualized, but two things about them matter to the
//! schedule: file I/O calls are hook events, and a blocking lock request
//! must not hold the baton. Which paths a guest may name is `hostfs`'s.

use std::ffi::{c_char, c_int, c_void};

use crate::sched::{self, my_id};

extern "C" {
    fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    #[link_name = "open$NOCANCEL"]
    pub fn open_nocancel(path: *const c_char, flags: c_int, ...) -> c_int;
}

/// Locks are released by an unlock, a close or a process death, and each
/// of those wakes the I/O waiters, so a blocked request retries exactly
/// when it could succeed.
pub unsafe extern "C" fn my_flock(fd: c_int, op: c_int) -> c_int {
    sched::hook_event(sched::SITE_FILE);
    if my_id().is_none() {
        return libc::flock(fd, op);
    }
    if op & libc::LOCK_UN != 0 {
        let rc = libc::flock(fd, op);
        crate::io::wake_io();
        return rc;
    }
    if op & libc::LOCK_NB != 0 {
        return libc::flock(fd, op);
    }
    loop {
        let rc = libc::flock(fd, op | libc::LOCK_NB);
        if rc == 0 || crate::errno::get() != libc::EWOULDBLOCK {
            return rc;
        }
        crate::io::park_for_io(None);
    }
}

/// `fcntl(F_SETLK)` and `fcntl(F_SETLKW)`.
pub unsafe fn record_lock(fd: c_int, wait: bool, lock: *mut libc::flock) -> c_int {
    let unlocking = !lock.is_null() && (*lock).l_type == libc::F_UNLCK as libc::c_short;
    loop {
        let rc = libc::fcntl(fd, libc::F_SETLK, lock);
        if rc == 0 && unlocking {
            crate::io::wake_io();
        }
        let busy =
            rc != 0 && (crate::errno::get() == libc::EAGAIN || crate::errno::get() == libc::EACCES);
        if !wait || !busy {
            return rc;
        }
        crate::io::park_for_io(None);
    }
}

pub unsafe extern "C" fn my_pread(fd: c_int, buf: *mut c_void, n: usize, off: i64) -> isize {
    sched::hook_event(sched::SITE_FILE);
    libc::pread(fd, buf, n, off)
}

pub unsafe extern "C" fn my_pwrite(fd: c_int, buf: *const c_void, n: usize, off: i64) -> isize {
    sched::hook_event(sched::SITE_FILE);
    libc::pwrite(fd, buf, n, off)
}

pub unsafe extern "C" fn my_lseek(fd: c_int, off: i64, whence: c_int) -> i64 {
    sched::hook_event(sched::SITE_FILE);
    libc::lseek(fd, off, whence)
}

pub unsafe extern "C" fn my_fsync(fd: c_int) -> c_int {
    sched::hook_event(sched::SITE_FILE);
    libc::fsync(fd)
}

/// `open` is variadic (the mode); see the `fcntl` shim.
#[no_mangle]
pub unsafe extern "C" fn rewrite_open_impl(
    path: *const c_char,
    flags: c_int,
    mode: usize,
) -> c_int {
    sched::hook_event(sched::SITE_FILE);
    if !crate::hostfs::permits("open", path) {
        return -1;
    }
    let fd = open(path, flags, mode as c_int);
    crate::determinism::opened(path, fd);
    fd
}

/// What stdio and the rest of libSystem call instead of `open`
#[no_mangle]
pub unsafe extern "C" fn rewrite_open_nocancel_impl(
    path: *const c_char,
    flags: c_int,
    mode: usize,
) -> c_int {
    sched::hook_event(sched::SITE_FILE);
    if !crate::hostfs::permits("open", path) {
        return -1;
    }
    let fd = open_nocancel(path, flags, mode as c_int);
    crate::determinism::opened(path, fd);
    fd
}

/// `openat(dirfd, path, flags, mode)`: the variadic mode is the fourth
/// argument, so the shim loads it into x3.
#[no_mangle]
pub unsafe extern "C" fn rewrite_openat_impl(
    dirfd: c_int,
    path: *const c_char,
    flags: c_int,
    mode: usize,
) -> c_int {
    sched::hook_event(sched::SITE_FILE);
    if !crate::hostfs::permits_at("openat", dirfd, path) {
        return -1;
    }
    let fd = libc::openat(dirfd, path, flags, mode as c_int);
    crate::determinism::opened(path, fd);
    fd
}

/// A POSIX shared memory name as this run gives it to the kernel: prefixed
/// with the launcher's pid, so that runs never meet each other's objects
/// (a name is machine-wide, and one a killed guest left behind would make
/// a create fail and the guest try another). None when it would not fit
/// the kernel's 31 characters, in which case the name stays as it is.
fn run_pshm_name(name: *const c_char) -> Option<std::ffi::CString> {
    let given = unsafe { std::ffi::CStr::from_ptr(name) }.to_bytes();
    let launcher = sched::shared().map_or(0, crate::shared::Shared::launcher_pid);
    let tag = if launcher == 0 {
        unsafe { libc::getpid() }
    } else {
        launcher
    };
    let mut out = format!("/r{:04x}", tag & 0xFFFF).into_bytes();
    out.extend_from_slice(given.strip_prefix(b"/").unwrap_or(given));
    (out.len() <= 31)
        .then(|| std::ffi::CString::new(out).ok())
        .flatten()
}

fn note_pshm(name: &std::ffi::CStr) {
    let bytes = name.to_bytes();
    sched::with(|s, _| {
        let n = s.npshm_names as usize;
        if n < crate::shared::MAX_IPC_OBJECTS && bytes.len() < 32 {
            s.pshm_names[n] = [0; 32];
            s.pshm_names[n][..bytes.len()].copy_from_slice(bytes);
            s.npshm_names += 1;
        }
    });
}

/// `shm_open` is variadic (the mode); see the `open` shim.
#[no_mangle]
pub unsafe extern "C" fn rewrite_shm_open_impl(
    name: *const c_char,
    oflag: c_int,
    mode: usize,
) -> c_int {
    sched::hook_event(sched::SITE_FILE);
    if sched::my_id().is_none() || name.is_null() {
        return libc::shm_open(name, oflag, mode as c_int);
    }
    let Some(ours) = run_pshm_name(name) else {
        return libc::shm_open(name, oflag, mode as c_int);
    };
    let fd = libc::shm_open(ours.as_ptr(), oflag, mode as c_int);
    if fd >= 0 && oflag & libc::O_CREAT != 0 {
        note_pshm(&ours);
    }
    fd
}

pub unsafe extern "C" fn my_shm_unlink(name: *const c_char) -> c_int {
    sched::hook_event(sched::SITE_FILE);
    if sched::my_id().is_none() || name.is_null() {
        return libc::shm_unlink(name);
    }
    match run_pshm_name(name) {
        Some(ours) => libc::shm_unlink(ours.as_ptr()),
        None => libc::shm_unlink(name),
    }
}

extern "C" {
    pub fn rewrite_open_shim();
    pub fn rewrite_shm_open_shim();
    pub fn rewrite_open_nocancel_shim();
    pub fn rewrite_openat_shim();
}

std::arch::global_asm!(
    ".globl _rewrite_open_shim",
    ".p2align 2",
    "_rewrite_open_shim:",
    "ldr x2, [sp]",
    "b _rewrite_open_impl",
    ".globl _rewrite_shm_open_shim",
    ".p2align 2",
    "_rewrite_shm_open_shim:",
    "ldr x2, [sp]",
    "b _rewrite_shm_open_impl",
    ".globl _rewrite_open_nocancel_shim",
    ".p2align 2",
    "_rewrite_open_nocancel_shim:",
    "ldr x2, [sp]",
    "b _rewrite_open_nocancel_impl",
    ".globl _rewrite_openat_shim",
    ".p2align 2",
    "_rewrite_openat_shim:",
    "ldr x3, [sp]",
    "b _rewrite_openat_impl",
);
