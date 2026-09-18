//! Blocking I/O on kernel objects that guests share (pipes, and sockets
//! that pass through to the kernel). A call that would block must not
//! hold the baton, so it becomes a readiness wait: try without blocking,
//! park as an I/O waiter if not ready, re-check when woken.
//!
//! Only the baton holder runs, so the state of these objects changes only
//! when a guest acts on them. Waking every I/O waiter after each write,
//! close and process death and letting them re-check is therefore exact.

use std::ffi::{c_int, c_void};

use crate::sched::{self, my_id, State};
use crate::shared;
use crate::spin::SpinLock;

extern "C" {
    #[link_name = "read$NOCANCEL"]
    pub fn read_nocancel(fd: c_int, buf: *mut c_void, n: usize) -> isize;
    #[link_name = "write$NOCANCEL"]
    pub fn write_nocancel(fd: c_int, buf: *const c_void, n: usize) -> isize;
    #[link_name = "readv$NOCANCEL"]
    pub fn readv_nocancel(fd: c_int, iov: *const libc::iovec, n: c_int) -> isize;
    #[link_name = "writev$NOCANCEL"]
    pub fn writev_nocancel(fd: c_int, iov: *const libc::iovec, n: c_int) -> isize;
    #[link_name = "close$NOCANCEL"]
    pub fn close_nocancel(fd: c_int) -> c_int;
}

/// `(st_dev, st_ino)` of descriptors whose peer is outside the run
static EXTERNAL: SpinLock<Vec<(i64, u64)>> = SpinLock::new(Vec::new());

pub fn init() {
    let Ok(list) = std::env::var(shared::EXTERNAL_VAR) else {
        return;
    };
    let parsed = list
        .split(',')
        .filter_map(|e| {
            let (dev, ino) = e.split_once(':')?;
            Some((dev.parse().ok()?, ino.parse().ok()?))
        })
        .collect();
    *EXTERNAL.lock() = parsed;
}

/// Whether blocking on `fd` is ours to turn into a readiness wait: a pipe
/// or socket between guests, in blocking mode, used by a scheduled thread.
fn managed(fd: c_int) -> bool {
    if my_id().is_none() {
        return false;
    }
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &raw mut st) } != 0 {
        return false;
    }
    let kind = st.st_mode & libc::S_IFMT;
    if kind != libc::S_IFIFO && kind != libc::S_IFSOCK {
        return false;
    }
    if EXTERNAL.lock().contains(&(i64::from(st.st_dev), st.st_ino)) {
        return false;
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    flags >= 0 && flags & libc::O_NONBLOCK == 0
}

/// A pipe or kernel socket whose other end is a guest's, whatever its
/// blocking mode: what `poll` may wait on in the scheduler.
pub fn is_guest_object(fd: c_int) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &raw mut st) } != 0 {
        return false;
    }
    let kind = st.st_mode & libc::S_IFMT;
    (kind == libc::S_IFIFO || kind == libc::S_IFSOCK)
        && !EXTERNAL.lock().contains(&(i64::from(st.st_dev), st.st_ino))
}

/// Any pipe or socket, for deciding whether an act can unblock a peer
fn shared_object(fd: c_int) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &raw mut st) } != 0 {
        return false;
    }
    let kind = st.st_mode & libc::S_IFMT;
    kind == libc::S_IFIFO || kind == libc::S_IFSOCK
}

/// Times a thread of this process parked for readiness, for the report
pub static IO_WAITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn park_for_io() {
    IO_WAITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    sched::yield_baton(State::Blocked(shared::IO_KEY as usize), shared::IO_KEY);
}

fn wake_io() {
    sched::with(|s, _| s.wake_io());
}

fn ready(fd: c_int, events: libc::c_short) -> bool {
    let mut p = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    // Errors and hangups count: the real call will report them
    unsafe { libc::poll(&raw mut p, 1, 0) != 0 }
}

fn errno() -> c_int {
    unsafe { *libc::__error() }
}

/// Run `f` once `fd` is readable.
fn when_readable(fd: c_int, f: impl Fn() -> isize) -> isize {
    if managed(fd) {
        while !ready(fd, libc::POLLIN) {
            park_for_io();
        }
    }
    let n = f();
    // Draining a full pipe lets its writer go on
    if n > 0 && my_id().is_some() && shared_object(fd) {
        wake_io();
    }
    n
}

/// Blocking write of the whole buffer without holding the baton while the
/// object is full. `O_NONBLOCK` is set only around the attempt; nobody
/// else runs in between, so other users of the open file never see it.
fn write_all(fd: c_int, buf: *const u8, len: usize) -> isize {
    let mut done = 0usize;
    while done < len {
        let n = unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            let n = libc::write(fd, buf.add(done).cast(), len - done);
            let e = errno();
            libc::fcntl(fd, libc::F_SETFL, flags);
            *libc::__error() = e;
            n
        };
        if n > 0 {
            done += n as usize;
            wake_io();
        } else if n < 0 && errno() == libc::EAGAIN {
            park_for_io();
        } else if n < 0 && errno() == libc::EINTR {
        } else {
            return if done > 0 { done as isize } else { n };
        }
    }
    done as isize
}

fn write_managed(fd: c_int, buf: *const c_void, n: usize, real: impl Fn() -> isize) -> isize {
    if managed(fd) {
        return write_all(fd, buf.cast(), n);
    }
    let r = real();
    if r > 0 && my_id().is_some() && shared_object(fd) {
        wake_io();
    }
    r
}

/// Scatter read from a virtual socket: fill the buffers in order and
/// stop at the first one that comes up short.
unsafe fn readv_virtual(fd: c_int, sock: u32, iov: *const libc::iovec, n: c_int) -> isize {
    let mut total = 0isize;
    for v in std::slice::from_raw_parts(iov, n.max(0) as usize) {
        // Only the first buffer may wait for data
        let flags = if total > 0 { libc::MSG_DONTWAIT } else { 0 };
        let got = crate::net::recv_fd(fd, sock, v.iov_base.cast(), v.iov_len, flags);
        if got < 0 {
            return if total > 0 { total } else { got };
        }
        total += got;
        if (got as usize) < v.iov_len {
            break;
        }
    }
    total
}

unsafe fn writev_virtual(fd: c_int, sock: u32, iov: *const libc::iovec, n: c_int) -> isize {
    let mut total = 0isize;
    for v in std::slice::from_raw_parts(iov, n.max(0) as usize) {
        let sent = crate::net::send_fd(fd, sock, v.iov_base.cast(), v.iov_len, 0);
        if sent < 0 {
            return if total > 0 { total } else { sent };
        }
        total += sent;
        if (sent as usize) < v.iov_len {
            break;
        }
    }
    total
}

pub unsafe extern "C" fn my_read(fd: c_int, buf: *mut c_void, n: usize) -> isize {
    if let Some(sock) = crate::net::lookup(fd) {
        return crate::net::recv_fd(fd, sock, buf.cast(), n, 0);
    }
    when_readable(fd, || unsafe { libc::read(fd, buf, n) })
}

pub unsafe extern "C" fn my_read_nocancel(fd: c_int, buf: *mut c_void, n: usize) -> isize {
    if let Some(sock) = crate::net::lookup(fd) {
        return crate::net::recv_fd(fd, sock, buf.cast(), n, 0);
    }
    when_readable(fd, || unsafe { read_nocancel(fd, buf, n) })
}

pub unsafe extern "C" fn my_readv(fd: c_int, iov: *const libc::iovec, n: c_int) -> isize {
    if let Some(sock) = crate::net::lookup(fd) {
        return readv_virtual(fd, sock, iov, n);
    }
    when_readable(fd, || unsafe { libc::readv(fd, iov, n) })
}

pub unsafe extern "C" fn my_readv_nocancel(fd: c_int, iov: *const libc::iovec, n: c_int) -> isize {
    if let Some(sock) = crate::net::lookup(fd) {
        return readv_virtual(fd, sock, iov, n);
    }
    when_readable(fd, || unsafe { readv_nocancel(fd, iov, n) })
}

pub unsafe extern "C" fn my_write(fd: c_int, buf: *const c_void, n: usize) -> isize {
    if let Some(sock) = crate::net::lookup(fd) {
        return crate::net::send_fd(fd, sock, buf.cast(), n, 0);
    }
    write_managed(fd, buf, n, || unsafe { libc::write(fd, buf, n) })
}

pub unsafe extern "C" fn my_write_nocancel(fd: c_int, buf: *const c_void, n: usize) -> isize {
    if let Some(sock) = crate::net::lookup(fd) {
        return crate::net::send_fd(fd, sock, buf.cast(), n, 0);
    }
    write_managed(fd, buf, n, || unsafe { write_nocancel(fd, buf, n) })
}

/// Vectors go out one buffer at a time; a pipe write above `PIPE_BUF` was
/// never atomic anyway.
unsafe fn writev_managed(
    fd: c_int,
    iov: *const libc::iovec,
    n: c_int,
    real: impl Fn() -> isize,
) -> isize {
    if let Some(sock) = crate::net::lookup(fd) {
        return writev_virtual(fd, sock, iov, n);
    }
    if !managed(fd) {
        let r = real();
        if r > 0 && my_id().is_some() && shared_object(fd) {
            wake_io();
        }
        return r;
    }
    let mut total = 0isize;
    for v in std::slice::from_raw_parts(iov, n.max(0) as usize) {
        let w = write_all(fd, v.iov_base.cast(), v.iov_len);
        if w < 0 {
            return if total > 0 { total } else { w };
        }
        total += w;
        if (w as usize) < v.iov_len {
            break;
        }
    }
    total
}

pub unsafe extern "C" fn my_writev(fd: c_int, iov: *const libc::iovec, n: c_int) -> isize {
    writev_managed(fd, iov, n, || unsafe { libc::writev(fd, iov, n) })
}

pub unsafe extern "C" fn my_writev_nocancel(fd: c_int, iov: *const libc::iovec, n: c_int) -> isize {
    writev_managed(fd, iov, n, || unsafe { writev_nocancel(fd, iov, n) })
}

fn close_managed(fd: c_int, real: impl Fn() -> c_int) -> c_int {
    if let Some(sock) = crate::net::lookup(fd) {
        let rc = real();
        if rc == 0 {
            crate::net::closed(sock);
        }
        return rc;
    }
    let wake = my_id().is_some() && shared_object(fd);
    let rc = real();
    // The last writer closing is a reader's EOF
    if wake {
        wake_io();
    }
    rc
}

pub unsafe extern "C" fn my_close(fd: c_int) -> c_int {
    close_managed(fd, || unsafe { libc::close(fd) })
}

pub unsafe extern "C" fn my_close_nocancel(fd: c_int) -> c_int {
    close_managed(fd, || unsafe { close_nocancel(fd) })
}
