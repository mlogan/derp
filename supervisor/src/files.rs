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
    open(path, flags, mode as c_int)
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
    open_nocancel(path, flags, mode as c_int)
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
    libc::openat(dirfd, path, flags, mode as c_int)
}

extern "C" {
    pub fn rewrite_open_shim();
    pub fn rewrite_open_nocancel_shim();
    pub fn rewrite_openat_shim();
}

std::arch::global_asm!(
    ".globl _rewrite_open_shim",
    ".p2align 2",
    "_rewrite_open_shim:",
    "ldr x2, [sp]",
    "b _rewrite_open_impl",
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
