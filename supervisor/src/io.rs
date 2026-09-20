//! Blocking I/O on kernel objects that guests share (pipes, and sockets
//! that pass through to the kernel). A call that would block must not
//! hold the baton, so it becomes a readiness wait: try without blocking,
//! park as an I/O waiter if not ready, re-check when woken.
//!
//! Only the baton holder runs, so the state of these objects changes only
//! when a guest acts on them. Waking every I/O waiter after each write,
//! close and process death and letting them re-check is therefore exact.

use std::ffi::{c_int, c_void};

use crate::sched::{self, my_id};
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
        // Padding entries of the fixed-width list
        .filter(|&(dev, ino): &(i64, u64)| dev != 0 || ino != 0)
        .collect();
    *EXTERNAL.lock() = parsed;
}

/// A kernel network socket: what `leave_virtual_network` left behind. Its
/// peer is outside the run, so it blocks for real. (`fstat` gives these
/// neither a device nor an inode.)
fn kernel_network_socket(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFSOCK && st.st_dev == 0 && st.st_ino == 0
}

/// A pipe or kernel socket whose other end is a guest's: its state only
/// changes when a guest acts, so waiting on it belongs in the scheduler.
pub fn is_guest_object(fd: c_int) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &raw mut st) } != 0 {
        return false;
    }
    let kind = st.st_mode & libc::S_IFMT;
    (kind == libc::S_IFIFO || kind == libc::S_IFSOCK)
        && !kernel_network_socket(&st)
        && !EXTERNAL.lock().contains(&(i64::from(st.st_dev), st.st_ino))
}

/// Whether a blocking call on `fd` becomes a readiness wait: a guest
/// object in blocking mode, used by a scheduled thread.
fn managed(fd: c_int) -> bool {
    if my_id().is_none() || !is_guest_object(fd) {
        return false;
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    flags >= 0 && flags & libc::O_NONBLOCK == 0
}

/// In the child of a `fork`: see `SpinLock::force_unlock`.
pub fn forked() {
    EXTERNAL.force_unlock();
}

/// Times a thread of this process parked for readiness, for the report
pub static IO_WAITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Park until some guest's act may have made a descriptor ready, or until
/// `deadline`. Returns true when the deadline ended the wait. May also
/// return early with nothing changed: callers look again.
pub fn park_for_io(deadline: Option<u64>) -> bool {
    IO_WAITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    sched::block_until(shared::IO_KEY, deadline)
}

/// Say once that a wait covers descriptors of the outside world too.
/// Nothing in the run announces their readiness, and a run that waited for
/// them could not be repeated, so they are not supported: only the guests'
/// side of such a wait ever ends it.
pub fn note_outside_in_wait() {
    static NOTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !NOTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        crate::report::log(
            "a wait mixes guest descriptors with ones from outside the run; \
             the outside ones will not wake it (unsupported: not repeatable)",
        );
    }
}

pub fn wake_io() {
    let outside = !sched::on_scheduled_thread();
    sched::with(|s, pid| {
        if outside {
            sched::note_outside_wake(s, pid);
        }
        s.wake_io();
    });
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

/// Run `f` once `fd` is readable.
pub fn when_readable(fd: c_int, f: impl Fn() -> isize) -> isize {
    if managed(fd) {
        while !ready(fd, libc::POLLIN) {
            park_for_io(None);
        }
    }
    let n = f();
    // Draining a full pipe lets its writer go on
    if n > 0 && my_id().is_some() && is_guest_object(fd) {
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
            let e = crate::errno::get();
            libc::fcntl(fd, libc::F_SETFL, flags);
            *libc::__error() = e;
            n
        };
        if n > 0 {
            done += n as usize;
            wake_io();
        } else if n < 0 && crate::errno::get() == libc::EAGAIN {
            park_for_io(None);
        } else if n < 0 && crate::errno::get() == libc::EINTR {
        } else {
            return if done > 0 { done as isize } else { n };
        }
    }
    done as isize
}

/// After a kernel-level send on `fd` returned `r`: a guest waiting for the
/// other end of a socket pair must look again. Also from a thread we do
/// not schedule, where a signal handler's self-pipe write may land.
pub fn sent(fd: c_int, r: isize) -> isize {
    if r > 0 && is_guest_object(fd) {
        wake_io();
    }
    r
}

fn write_managed(fd: c_int, buf: *const c_void, n: usize, real: impl Fn() -> isize) -> isize {
    if managed(fd) {
        return write_all(fd, buf.cast(), n);
    }
    let r = real();
    // Also from a thread we do not schedule (a signal handler's self-pipe
    // write lands on whichever thread the kernel chose)
    if r > 0 && is_guest_object(fd) {
        wake_io();
    }
    r
}

/// Scatter read from a virtual socket: fill the buffers in order and
/// stop at the first one that comes up short.
unsafe fn readv_virtual(fd: c_int, sock: u32, iov: *const libc::iovec, n: c_int) -> isize {
    let vectors = std::slice::from_raw_parts(iov, n.max(0) as usize);
    if crate::net::is_datagram(sock) {
        // One datagram, scattered: reading per vector would take one each
        let room: usize = vectors.iter().map(|v| v.iov_len).sum();
        let mut data = vec![0u8; room];
        let got = crate::net::recv_fd(fd, sock, data.as_mut_ptr(), room, 0);
        let mut rest = &data[..got.max(0) as usize];
        for v in vectors {
            let k = rest.len().min(v.iov_len);
            std::ptr::copy_nonoverlapping(rest.as_ptr(), v.iov_base.cast::<u8>(), k);
            rest = &rest[k..];
        }
        return got;
    }
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
    if crate::net::is_datagram(sock) {
        // One datagram, gathered
        let mut data = Vec::new();
        for v in std::slice::from_raw_parts(iov, n.max(0) as usize) {
            data.extend_from_slice(std::slice::from_raw_parts(
                v.iov_base.cast::<u8>(),
                v.iov_len,
            ));
        }
        return crate::net::send_fd(fd, sock, data.as_ptr(), data.len(), 0);
    }
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
    sched::hook_event(sched::SITE_IO);
    if let Some(sock) = crate::net::lookup(fd) {
        return crate::net::recv_fd(fd, sock, buf.cast(), n, 0);
    }
    when_readable(fd, || unsafe { libc::read(fd, buf, n) })
}

pub unsafe extern "C" fn my_read_nocancel(fd: c_int, buf: *mut c_void, n: usize) -> isize {
    sched::hook_event(sched::SITE_IO);
    if let Some(sock) = crate::net::lookup(fd) {
        return crate::net::recv_fd(fd, sock, buf.cast(), n, 0);
    }
    when_readable(fd, || unsafe { read_nocancel(fd, buf, n) })
}

pub unsafe extern "C" fn my_readv(fd: c_int, iov: *const libc::iovec, n: c_int) -> isize {
    sched::hook_event(sched::SITE_IO);
    if let Some(sock) = crate::net::lookup(fd) {
        return readv_virtual(fd, sock, iov, n);
    }
    when_readable(fd, || unsafe { libc::readv(fd, iov, n) })
}

pub unsafe extern "C" fn my_readv_nocancel(fd: c_int, iov: *const libc::iovec, n: c_int) -> isize {
    sched::hook_event(sched::SITE_IO);
    if let Some(sock) = crate::net::lookup(fd) {
        return readv_virtual(fd, sock, iov, n);
    }
    when_readable(fd, || unsafe { readv_nocancel(fd, iov, n) })
}

pub unsafe extern "C" fn my_write(fd: c_int, buf: *const c_void, n: usize) -> isize {
    sched::hook_event(sched::SITE_IO);
    if let Some(sock) = crate::net::lookup(fd) {
        return crate::net::send_fd(fd, sock, buf.cast(), n, 0);
    }
    write_managed(fd, buf, n, || unsafe { libc::write(fd, buf, n) })
}

pub unsafe extern "C" fn my_write_nocancel(fd: c_int, buf: *const c_void, n: usize) -> isize {
    sched::hook_event(sched::SITE_IO);
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
        if r > 0 && is_guest_object(fd) {
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
    sched::hook_event(sched::SITE_IO);
    writev_managed(fd, iov, n, || unsafe { libc::writev(fd, iov, n) })
}

pub unsafe extern "C" fn my_writev_nocancel(fd: c_int, iov: *const libc::iovec, n: c_int) -> isize {
    sched::hook_event(sched::SITE_IO);
    writev_managed(fd, iov, n, || unsafe { writev_nocancel(fd, iov, n) })
}

fn close_managed(fd: c_int, real: impl Fn() -> c_int) -> c_int {
    // "Close everything above 2" (Python's subprocess does it before exec)
    // must not take the launcher's socket: spawning needs it.
    if fd == shared::COORD_FD && crate::coord::connected() {
        return 0;
    }
    if my_id().is_some() {
        crate::kq::closed(fd);
    }
    if let Some(sock) = crate::net::lookup(fd) {
        let rc = real();
        if rc == 0 {
            crate::net::closed(sock);
        }
        return rc;
    }
    let rc = real();
    // The last writer closing is a reader's EOF, and a closed file gives
    // up its locks
    if my_id().is_some() {
        wake_io();
    }
    rc
}

pub unsafe extern "C" fn my_close(fd: c_int) -> c_int {
    sched::hook_event(sched::SITE_IO);
    close_managed(fd, || unsafe { libc::close(fd) })
}

pub unsafe extern "C" fn my_close_nocancel(fd: c_int) -> c_int {
    sched::hook_event(sched::SITE_IO);
    close_managed(fd, || unsafe { close_nocancel(fd) })
}
