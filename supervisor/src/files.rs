//! Files are not virtualized, but three things about them matter to the
//! schedule: file I/O calls are hook events, a blocking lock request must
//! not hold the baton, and paths outside the run's scratch directory are
//! input worth knowing about.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::sched::{self, my_id};
use crate::shared;
use crate::spin::SpinLock;

extern "C" {
    fn open(path: *const c_char, flags: c_int, ...) -> c_int;
}

fn park_for_lock() {
    crate::io::IO_WAITS.fetch_add(1, Ordering::Relaxed);
    sched::block_until(shared::IO_KEY, None);
}

fn errno() -> c_int {
    unsafe { *libc::__error() }
}

/// Locks are released by an unlock, a close or a process death, and each
/// of those wakes the I/O waiters, so a blocked request retries exactly
/// when it could succeed.
pub unsafe extern "C" fn my_flock(fd: c_int, op: c_int) -> c_int {
    sched::hook_event(sched::SITE_FILE);
    if my_id().is_none() || op & libc::LOCK_NB != 0 {
        return libc::flock(fd, op);
    }
    if op & libc::LOCK_UN != 0 {
        let rc = libc::flock(fd, op);
        sched::with(|s, _| s.wake_io());
        return rc;
    }
    loop {
        let rc = libc::flock(fd, op | libc::LOCK_NB);
        if rc == 0 || errno() != libc::EWOULDBLOCK {
            return rc;
        }
        park_for_lock();
    }
}

/// `fcntl(F_SETLK)` and `fcntl(F_SETLKW)`.
pub unsafe fn record_lock(fd: c_int, wait: bool, lock: *mut libc::flock) -> c_int {
    let unlocking = !lock.is_null() && (*lock).l_type == libc::F_UNLCK as libc::c_short;
    loop {
        let rc = libc::fcntl(fd, libc::F_SETLK, lock);
        if rc == 0 && unlocking {
            sched::with(|s, _| s.wake_io());
        }
        let busy = rc != 0 && (errno() == libc::EAGAIN || errno() == libc::EACCES);
        if !wait || !busy {
            return rc;
        }
        park_for_lock();
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

/// Set by the launcher for manifest runs
const SCRATCH_VAR: &str = "REWRITE_SCRATCH";
static SCRATCH: SpinLock<Option<Vec<u8>>> = SpinLock::new(None);
static OUTSIDE_LOGGED: AtomicBool = AtomicBool::new(false);

pub fn init() {
    if let Ok(dir) = std::env::var(SCRATCH_VAR) {
        *SCRATCH.lock() = Some(dir.into_bytes());
    }
}

/// System locations every program touches through libSystem
fn ambient(path: &[u8]) -> bool {
    [
        &b"/dev/"[..],
        b"/usr/",
        b"/System/",
        b"/Library/",
        b"/private/etc/",
        b"/etc/",
        b"/var/db/",
    ]
    .iter()
    .any(|p| path.starts_with(p))
}

unsafe fn note_path(path: *const c_char) {
    if path.is_null() || OUTSIDE_LOGGED.load(Ordering::Relaxed) || my_id().is_none() {
        return;
    }
    let bytes = CStr::from_ptr(path).to_bytes();
    // Relative paths resolve against the scratch directory the run started in
    if !bytes.starts_with(b"/") || ambient(bytes) {
        return;
    }
    let inside = match SCRATCH.lock().as_ref() {
        Some(dir) => bytes.starts_with(dir),
        None => return,
    };
    if !inside && !OUTSIDE_LOGGED.swap(true, Ordering::Relaxed) {
        let mut line =
            String::from("file outside the scratch directory (its contents are input): ");
        line.push_str(&String::from_utf8_lossy(bytes));
        crate::report::log(&line);
    }
}

/// `open` is variadic (the mode); see the `fcntl` shim.
#[no_mangle]
pub unsafe extern "C" fn rewrite_open_impl(
    path: *const c_char,
    flags: c_int,
    mode: usize,
) -> c_int {
    sched::hook_event(sched::SITE_FILE);
    note_path(path);
    open(path, flags, mode as c_int)
}

extern "C" {
    pub fn rewrite_open_shim();
}

std::arch::global_asm!(
    ".globl _rewrite_open_shim",
    ".p2align 2",
    "_rewrite_open_shim:",
    "ldr x2, [sp]",
    "b _rewrite_open_impl",
);
