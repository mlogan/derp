//! Holds a guest's path names to its virtual host's directory. This is a
//! guard against configurations that would let hosts share files by
//! accident (an absolute path into another host's directory, a
//! `../other-host/data`), not a sandbox: paths are normalized lexically
//! and symlinks are not resolved, so whoever wants out gets out.
//!
//! A path may also be under a system location every program reads through
//! libSystem, or under one of the run file's `allow:` entries. Program
//! images are not checked: `execve` and `posix_spawn` name binaries that
//! live outside every host's directory.

use std::ffi::{c_char, c_int, CStr};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::shared;
use crate::spin::SpinLock;

struct Policy {
    root: Vec<u8>,
    allow: Vec<Vec<u8>>,
    host: String,
}

static POLICY: SpinLock<Option<Policy>> = SpinLock::new(None);
static REFUSED: AtomicU32 = AtomicU32::new(0);
/// Whether there is a policy at all: single-program runs have none, and
/// their path calls should cost nothing
static ACTIVE: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Set while this thread is inside `permits`. libSystem's `getcwd`
    /// opens "." itself, which lands back in our interposers; that inner
    /// call must pass, or the check would recurse (or spin on the lock).
    static CHECKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
const MAX_LOGGED: u32 = 20;

/// Read by every program through libSystem (time zones, locales, the
/// resolver, devices), or where toolchains and package managers install
/// what guests link and load. `/var/folders` holds the per-user caches
/// libSystem itself writes.
const SYSTEM: [&[u8]; 12] = [
    b"/usr",
    b"/System",
    b"/Library",
    b"/bin",
    b"/sbin",
    b"/dev",
    b"/etc",
    b"/var/db",
    b"/var/run",
    b"/var/folders",
    b"/opt/homebrew",
    b"/Applications/Xcode",
];

/// `/tmp`, `/var` and `/etc` are symlinks into `/private`; compare one form.
fn unprivate(path: &[u8]) -> &[u8] {
    match path.strip_prefix(b"/private") {
        Some(rest)
            if [&b"/tmp"[..], b"/var", b"/etc"]
                .iter()
                .any(|p| under(rest, p)) =>
        {
            rest
        }
        _ => path,
    }
}

fn under(path: &[u8], dir: &[u8]) -> bool {
    dir == b"/"
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.is_empty() || rest[0] == b'/')
}

/// Absolute and without `.`, `..` or doubled slashes. Lexical only.
fn normalize(path: &[u8], cwd: &[u8]) -> Vec<u8> {
    let mut parts: Vec<&[u8]> = Vec::new();
    let bases: [&[u8]; 2] = if path.starts_with(b"/") {
        [b"", path]
    } else {
        [cwd, path]
    };
    for base in bases {
        for part in base.split(|&b| b == b'/') {
            match part {
                b"" | b"." => {}
                b".." => {
                    parts.pop();
                }
                _ => parts.push(part),
            }
        }
    }
    let mut out = Vec::new();
    for part in parts {
        out.push(b'/');
        out.extend_from_slice(part);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

pub fn init() {
    let Ok(root) = std::env::var(shared::HOST_ROOT_VAR) else {
        return;
    };
    if root.is_empty() {
        return;
    }
    let allow = std::env::var(shared::ALLOW_VAR)
        .unwrap_or_default()
        .split(':')
        .filter(|a| !a.is_empty())
        .map(|a| unprivate(&normalize(a.as_bytes(), b"/")).to_vec())
        .collect();
    let host = root.rsplit('/').next().unwrap_or("").to_string();
    *POLICY.lock() = Some(Policy {
        root: unprivate(&normalize(root.as_bytes(), b"/")).to_vec(),
        allow,
        host,
    });
    ACTIVE.store(true, Ordering::Relaxed);
}

fn cwd() -> Vec<u8> {
    let mut buf = [0 as c_char; libc::PATH_MAX as usize];
    if unsafe { libc::getcwd(buf.as_mut_ptr(), buf.len()) }.is_null() {
        return b"/".to_vec();
    }
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_bytes().to_vec()
}

/// Run the supervisor's own path lookups (resolving a program to spawn)
/// without the guard: they are not the guest's.
pub fn exempt<T>(f: impl FnOnce() -> T) -> T {
    let was = CHECKING.with(|c| c.replace(true));
    let out = f();
    CHECKING.with(|c| c.set(was));
    out
}

/// Whether `path` is this host's to touch. Refusals set `EACCES` (`ENOENT`
/// for calls that only ask) and are logged with the call, so a
/// misconfiguration is visible, not silent.
pub unsafe fn permits(call: &str, path: *const c_char) -> bool {
    check(call, path, false)
}

/// For calls that only ask about a path (`stat`, `access`, `readlink`):
/// the directories above anything permitted are fine too. Programs walk
/// them (`realpath` on `/var/...` reads the `/var` symlink; `mkdir -p`),
/// and they say nothing about another host.
pub unsafe fn permits_metadata(call: &str, path: *const c_char) -> bool {
    check(call, path, true)
}

unsafe fn check(call: &str, path: *const c_char, ancestors: bool) -> bool {
    if path.is_null() || !ACTIVE.load(Ordering::Relaxed) || CHECKING.with(std::cell::Cell::get) {
        return true;
    }
    let given = CStr::from_ptr(path).to_bytes();
    // Before the lock: `getcwd` re-enters the interposers
    let here = if given.starts_with(b"/") {
        Vec::new()
    } else {
        CHECKING.with(|c| c.set(true));
        let here = cwd();
        CHECKING.with(|c| c.set(false));
        here
    };
    let guard = POLICY.lock();
    let Some(policy) = guard.as_ref() else {
        return true;
    };
    let absolute = normalize(given, &here);
    let seen = unprivate(&absolute);
    // CoreFoundation reads this from the real home directory (from the
    // password database, not `HOME`) in every program that links it.
    let ambient = seen.ends_with(b"/.CFUserTextEncoding") || ancestors && seen == b"/private";
    let ok = ambient
        || under(seen, &policy.root)
        || ancestors
            && (under(&policy.root, seen)
                || SYSTEM.iter().any(|sys| under(sys, seen))
                || policy.allow.iter().any(|a| under(a, seen)))
        || SYSTEM.iter().any(|s| under(seen, s))
        || policy.allow.iter().any(|a| under(seen, a));
    if !ok && ancestors {
        // Only asking. On this host the path does not exist; that is worth
        // a log line only if it exists for real (a probe for a file that is
        // nowhere is not a misconfiguration).
        drop(guard);
        let mut st: libc::stat = std::mem::zeroed();
        let exists = exempt(|| libc::lstat(path, &raw mut st)) == 0;
        if exists {
            refuse(call, given);
        }
        *libc::__error() = libc::ENOENT;
        return false;
    }
    if !ok {
        drop(guard);
        refuse(call, given);
        *libc::__error() = libc::EACCES;
    }
    ok
}

fn refuse(call: &str, given: &[u8]) {
    let guard = POLICY.lock();
    let Some(policy) = guard.as_ref() else { return };
    {
        if REFUSED.fetch_add(1, Ordering::Relaxed) < MAX_LOGGED {
            let mut line = String::new();
            let _ = std::fmt::Write::write_fmt(
                &mut line,
                format_args!(
                    "host {}: {call} {} refused: outside the host's directory",
                    policy.host,
                    String::from_utf8_lossy(given)
                ),
            );
            crate::report::log(&line);
        }
    }
}

pub fn refused() -> u32 {
    REFUSED.load(Ordering::Relaxed)
}

/// Interposer for a call whose first argument is the path.
macro_rules! guarded {
    ($name:ident, $check:ident, $real:ident, $fail:expr, ($($arg:ident: $ty:ty),*) -> $ret:ty) => {
        pub unsafe extern "C" fn $name(path: *const c_char, $($arg: $ty),*) -> $ret {
            if !$check(stringify!($real), path) {
                return $fail;
            }
            libc::$real(path, $($arg),*)
        }
    };
}

guarded!(my_stat, permits_metadata, stat, -1, (buf: *mut libc::stat) -> c_int);
guarded!(my_lstat, permits_metadata, lstat, -1, (buf: *mut libc::stat) -> c_int);
guarded!(my_access, permits_metadata, access, -1, (mode: c_int) -> c_int);
guarded!(my_mkdir, permits, mkdir, -1, (mode: libc::mode_t) -> c_int);
guarded!(my_rmdir, permits, rmdir, -1, () -> c_int);
guarded!(my_unlink, permits, unlink, -1, () -> c_int);
guarded!(my_chdir, permits, chdir, -1, () -> c_int);
guarded!(my_truncate, permits, truncate, -1, (len: libc::off_t) -> c_int);
guarded!(my_chmod, permits, chmod, -1, (mode: libc::mode_t) -> c_int);
guarded!(my_chown, permits, chown, -1, (uid: libc::uid_t, gid: libc::gid_t) -> c_int);
guarded!(my_utimes, permits, utimes, -1, (times: *const libc::timeval) -> c_int);
guarded!(my_mkfifo, permits, mkfifo, -1, (mode: libc::mode_t) -> c_int);
guarded!(my_creat, permits, creat, -1, (mode: libc::mode_t) -> c_int);
guarded!(my_readlink, permits_metadata, readlink, -1, (buf: *mut c_char, len: usize) -> isize);
guarded!(my_opendir, permits, opendir, std::ptr::null_mut(), () -> *mut libc::DIR);

pub unsafe extern "C" fn my_rename(from: *const c_char, to: *const c_char) -> c_int {
    if !permits("rename", from) || !permits("rename", to) {
        return -1;
    }
    libc::rename(from, to)
}

pub unsafe extern "C" fn my_link(from: *const c_char, to: *const c_char) -> c_int {
    if !permits("link", from) || !permits("link", to) {
        return -1;
    }
    libc::link(from, to)
}

/// Only where the link is made is checked: its target is just text.
pub unsafe extern "C" fn my_symlink(target: *const c_char, at: *const c_char) -> c_int {
    if !permits("symlink", at) {
        return -1;
    }
    libc::symlink(target, at)
}

/// A path relative to a directory descriptor was checked when that
/// directory was opened.
pub unsafe fn permits_at(call: &str, dirfd: c_int, path: *const c_char) -> bool {
    let relative_to_fd = dirfd != libc::AT_FDCWD && !path.is_null() && *path.cast::<u8>() != b'/';
    relative_to_fd || permits(call, path)
}

unsafe fn permits_metadata_at(call: &str, dirfd: c_int, path: *const c_char) -> bool {
    let relative_to_fd = dirfd != libc::AT_FDCWD && !path.is_null() && *path.cast::<u8>() != b'/';
    relative_to_fd || permits_metadata(call, path)
}

pub unsafe extern "C" fn my_fstatat(
    dirfd: c_int,
    path: *const c_char,
    buf: *mut libc::stat,
    flags: c_int,
) -> c_int {
    if !permits_metadata_at("fstatat", dirfd, path) {
        return -1;
    }
    libc::fstatat(dirfd, path, buf, flags)
}

pub unsafe extern "C" fn my_unlinkat(dirfd: c_int, path: *const c_char, flags: c_int) -> c_int {
    if !permits_at("unlinkat", dirfd, path) {
        return -1;
    }
    libc::unlinkat(dirfd, path, flags)
}

pub unsafe extern "C" fn my_mkdirat(
    dirfd: c_int,
    path: *const c_char,
    mode: libc::mode_t,
) -> c_int {
    if !permits_at("mkdirat", dirfd, path) {
        return -1;
    }
    libc::mkdirat(dirfd, path, mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_normalized_lexically() {
        assert_eq!(normalize(b"/a/./b//c/../d", b"/"), b"/a/b/d");
        assert_eq!(normalize(b"../beta/data", b"/run/alpha"), b"/run/beta/data");
        assert_eq!(normalize(b"x", b"/run/alpha/"), b"/run/alpha/x");
        assert_eq!(normalize(b"/../..", b"/"), b"/");
        assert_eq!(normalize(b"", b"/run/alpha"), b"/run/alpha");
    }

    #[test]
    fn a_directory_contains_itself_and_what_is_below() {
        assert!(under(b"/run/alpha", b"/run/alpha"));
        assert!(under(b"/run/alpha/x", b"/run/alpha"));
        assert!(!under(b"/run/alphabet", b"/run/alpha"));
        assert!(!under(b"/run", b"/run/alpha"));
        assert!(under(b"/run/alpha", b"/"));
        assert!(under(b"/", b"/"));
    }

    #[test]
    fn private_aliases_compare_equal() {
        assert_eq!(unprivate(b"/private/tmp/x"), b"/tmp/x");
        assert_eq!(unprivate(b"/private/var/folders/a"), b"/var/folders/a");
        assert_eq!(unprivate(b"/private/other"), b"/private/other");
        assert_eq!(unprivate(b"/tmp/x"), b"/tmp/x");
    }
}
