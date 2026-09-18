//! Socket API over the virtual network in the shared state
//! (`shared::netstate`). `AF_INET` and `AF_UNIX` stream sockets made by a
//! scheduled thread never reach the kernel's network stack; blocking calls
//! become readiness waits like pipe I/O.
//!
//! The descriptor a guest holds is a real `AF_UNIX` socket that is never
//! connected. It keeps descriptor numbers, `dup`, `fork`, close-on-exec
//! and `O_NONBLOCK` coherent for free, and its `st_ino` names the virtual
//! socket, so any process can tell what a descriptor is with one `fstat`.

use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::sched::{self, my_id};
use crate::shared::netstate::{
    Addr, NetError, FAMILY_INET, FAMILY_UNIX, KIND_DGRAM, KIND_STREAM, MAX_DGRAM, RING,
    UNIX_PATH_MAX,
};
use crate::shared::{self};

type Sockaddr = libc::sockaddr;
type Socklen = libc::socklen_t;

fn set_errno(e: c_int) -> c_int {
    unsafe { *libc::__error() = e };
    -1
}

/// Sockets are virtual only for scheduled threads of a launcher's run.
fn active() -> bool {
    my_id().is_some() && crate::coord::connected()
}

/// `st_ino` of `fd` if it could be one of our placeholders
fn ident_of(fd: c_int) -> Option<u64> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &raw mut st) } != 0 {
        return None;
    }
    let unix_socket = st.st_mode & libc::S_IFMT == libc::S_IFSOCK && st.st_dev == -1;
    (unix_socket && st.st_ino != 0).then_some(st.st_ino)
}

/// The virtual socket behind `fd`, if any.
pub fn lookup(fd: c_int) -> Option<u32> {
    if !active() {
        return None;
    }
    let ident = ident_of(fd)?;
    sched::with(|s, _| s.net.by_ident(ident)).flatten()
}

fn kind_of(sock: u32) -> u8 {
    sched::with(|s, _| s.net.socks[sock as usize].kind).unwrap_or(KIND_STREAM)
}

fn nonblocking(fd: c_int) -> bool {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    flags >= 0 && flags & libc::O_NONBLOCK != 0
}

/// Which of a socket's timeouts bounds a wait
#[derive(Clone, Copy)]
enum Wait {
    /// Fail with `EAGAIN` instead of waiting
    Never,
    Receive(u32),
    Send(u32),
}

/// Run `op` on the locked state until it is ready, parking in between
/// unless the descriptor is non-blocking. The socket's receive or send
/// timeout is a virtual-time deadline for the whole call. Success wakes
/// the I/O waiters: whatever happened may have made a peer ready.
fn blocking<T>(
    fd: c_int,
    wait: Wait,
    mut op: impl FnMut(&mut shared::State, u32) -> Result<T, NetError>,
) -> Result<T, c_int> {
    let timeout = sched::with(|s, _| match wait {
        Wait::Receive(sock) => s.net.socks[sock as usize].rcv_timeout_ns,
        Wait::Send(sock) => s.net.socks[sock as usize].snd_timeout_ns,
        Wait::Never => 0,
    })
    .unwrap_or(0);
    let deadline = (timeout != 0).then(|| sched::now() + timeout);
    let dontwait = matches!(wait, Wait::Never);
    loop {
        let r = sched::with(|s, pid| {
            let r = op(s, pid);
            if r.is_ok() {
                s.wake_io();
            }
            r
        })
        .unwrap_or(Err(NetError::Errno(libc::EBADF)));
        match r {
            Ok(v) => return Ok(v),
            Err(NetError::Errno(e)) => return Err(e),
            Err(NetError::WouldBlock) if dontwait || nonblocking(fd) => return Err(libc::EAGAIN),
            Err(NetError::WouldBlock) => {
                crate::io::IO_WAITS.fetch_add(1, Ordering::Relaxed);
                if sched::block_until(shared::IO_KEY, deadline) {
                    return Err(libc::EAGAIN);
                }
            }
        }
    }
}

fn status(r: Result<(), c_int>) -> c_int {
    match r {
        Ok(()) => 0,
        Err(e) => set_errno(e),
    }
}

// ---- addresses ------------------------------------------------------------

unsafe fn parse_addr(addr: *const Sockaddr, len: Socklen) -> Option<Addr> {
    if addr.is_null() || (len as usize) < 2 {
        return None;
    }
    match c_int::from((*addr).sa_family) {
        libc::AF_INET if len as usize >= std::mem::size_of::<libc::sockaddr_in>() => {
            // The caller's buffer need not be aligned
            let a = addr.cast::<libc::sockaddr_in>().read_unaligned();
            Some(Addr::inet(
                u32::from_be(a.sin_addr.s_addr),
                u16::from_be(a.sin_port),
            ))
        }
        libc::AF_UNIX => {
            let bytes = std::slice::from_raw_parts(addr.cast::<u8>().add(2), len as usize - 2);
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            Some(Addr::unix(&bytes[..end.min(UNIX_PATH_MAX)]))
        }
        _ => None,
    }
}

/// Write `a` the way the socket calls return addresses: truncated to the
/// caller's buffer, with the full length reported.
unsafe fn store_addr(a: &Addr, out: *mut Sockaddr, len: *mut Socklen) {
    if out.is_null() || len.is_null() {
        return;
    }
    let mut buf = [0u8; 2 + UNIX_PATH_MAX];
    let full = if a.family == FAMILY_INET {
        let mut sin: libc::sockaddr_in = std::mem::zeroed();
        sin.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
        sin.sin_family = libc::AF_INET as u8;
        sin.sin_port = a.port.to_be();
        sin.sin_addr.s_addr = a.ip.to_be();
        let n = std::mem::size_of::<libc::sockaddr_in>();
        std::ptr::copy_nonoverlapping((&raw const sin).cast::<u8>(), buf.as_mut_ptr(), n);
        n
    } else {
        let path = a.path();
        let n = (2 + path.len() + 1).min(buf.len());
        buf[0] = n as u8;
        buf[1] = libc::AF_UNIX as u8;
        buf[2..2 + path.len()].copy_from_slice(path);
        n
    };
    let n = full.min(*len as usize);
    std::ptr::copy_nonoverlapping(buf.as_ptr(), out.cast::<u8>(), n);
    *len = full as Socklen;
}

// ---- lifecycle ------------------------------------------------------------

/// A fresh placeholder descriptor and its identity
fn placeholder(ty: c_int) -> Result<(c_int, u64), c_int> {
    let fd = unsafe { libc::socket(libc::AF_UNIX, ty, 0) };
    if fd < 0 {
        return Err(unsafe { *libc::__error() });
    }
    if let Some(ident) = ident_of(fd) {
        return Ok((fd, ident));
    }
    unsafe { libc::close(fd) };
    Err(libc::ENOTSOCK)
}

pub unsafe extern "C" fn my_socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int {
    let family = match domain {
        libc::AF_INET => FAMILY_INET,
        libc::AF_UNIX => FAMILY_UNIX,
        _ => 0,
    };
    let kind = match ty {
        libc::SOCK_STREAM => KIND_STREAM,
        libc::SOCK_DGRAM => KIND_DGRAM,
        _ => 0,
    };
    if !active() || family == 0 || kind == 0 {
        return libc::socket(domain, ty, protocol);
    }
    let (fd, ident) = match placeholder(ty) {
        Ok(p) => p,
        Err(e) => return set_errno(e),
    };
    let made = sched::with(|s, pid| {
        let host = s.procs[pid as usize].host;
        let sock = s.net.socket(host, family, kind)?;
        s.net.set_ident(sock, ident);
        s.net.add_ref(pid, sock);
        Ok::<(), NetError>(())
    });
    if !matches!(made, Some(Ok(()))) {
        libc::close(fd);
        return set_errno(libc::ENFILE);
    }
    fd
}

pub unsafe extern "C" fn my_bind(fd: c_int, addr: *const Sockaddr, len: Socklen) -> c_int {
    let Some(sock) = lookup(fd) else {
        return libc::bind(fd, addr, len);
    };
    let Some(a) = parse_addr(addr, len) else {
        return set_errno(libc::EAFNOSUPPORT);
    };
    status(blocking(fd, Wait::Never, |s, _| s.net.bind(sock, a)))
}

pub unsafe extern "C" fn my_listen(fd: c_int, backlog: c_int) -> c_int {
    let Some(sock) = lookup(fd) else {
        return libc::listen(fd, backlog);
    };
    status(blocking(fd, Wait::Never, |s, _| {
        s.net.listen(sock, backlog)
    }))
}

static PASSTHROUGH_LOGGED: AtomicBool = AtomicBool::new(false);

/// The destination is outside the virtual network: swap the placeholder
/// for a kernel socket at the same descriptor. What comes back over it is
/// input to the run.
unsafe fn leave_virtual_network(fd: c_int, sock: u32, domain: c_int) -> bool {
    let ty = if kind_of(sock) == KIND_DGRAM {
        libc::SOCK_DGRAM
    } else {
        libc::SOCK_STREAM
    };
    let real = libc::socket(domain, ty, 0);
    if real < 0 {
        return false;
    }
    let flags = libc::fcntl(fd, libc::F_GETFL);
    let cloexec = libc::fcntl(fd, libc::F_GETFD);
    let ok = libc::dup2(real, fd) == fd;
    libc::close(real);
    if !ok {
        return false;
    }
    libc::fcntl(fd, libc::F_SETFL, flags);
    libc::fcntl(fd, libc::F_SETFD, cloexec);
    sched::with(|s, pid| {
        s.net.drop_ref(pid, sock);
        s.net.passthrough += 1;
    });
    if !PASSTHROUGH_LOGGED.swap(true, Ordering::Relaxed) {
        crate::report::log(
            "connection to an address outside the virtual network; its traffic is input",
        );
    }
    true
}

/// A UNIX-domain path that is a socket in the real filesystem belongs to
/// the system, not to a guest: libSystem's resolver reaches mDNSResponder
/// this way, on the guest's own thread.
unsafe fn is_system_socket(addr: &Addr) -> bool {
    let Ok(path) = std::ffi::CString::new(addr.path()) else {
        return false;
    };
    let mut st: libc::stat = std::mem::zeroed();
    libc::stat(path.as_ptr(), &raw mut st) == 0 && st.st_mode & libc::S_IFMT == libc::S_IFSOCK
}

pub unsafe extern "C" fn my_connect(fd: c_int, addr: *const Sockaddr, len: Socklen) -> c_int {
    let Some(sock) = lookup(fd) else {
        return libc::connect(fd, addr, len);
    };
    let Some(dest) = parse_addr(addr, len) else {
        return set_errno(libc::EAFNOSUPPORT);
    };
    let dgram = kind_of(sock) == KIND_DGRAM;
    let mut outside = false;
    let r = blocking(fd, Wait::Never, |s, pid| {
        let here = s.procs[pid as usize].host;
        let host = if dest.family == FAMILY_INET {
            match s.net.host_for_ip(here, dest.ip) {
                Ok(h) => h,
                Err(true) => return Err(NetError::Errno(libc::EHOSTUNREACH)),
                Err(false) => {
                    outside = true;
                    return Err(NetError::Errno(libc::ENETUNREACH));
                }
            }
        } else {
            here
        };
        if dgram {
            s.net.connect_dgram(sock, dest)
        } else {
            s.net.connect(sock, host, &dest)
        }
    });
    if dest.family == FAMILY_UNIX && r == Err(libc::ECONNREFUSED) && is_system_socket(&dest) {
        outside = true;
    }
    if outside {
        let domain = if dest.family == FAMILY_UNIX {
            libc::AF_UNIX
        } else {
            libc::AF_INET
        };
        if leave_virtual_network(fd, sock, domain) {
            return libc::connect(fd, addr, len);
        }
        return set_errno(libc::ENETUNREACH);
    }
    status(r)
}

pub unsafe extern "C" fn my_accept(fd: c_int, addr: *mut Sockaddr, len: *mut Socklen) -> c_int {
    let Some(listener) = lookup(fd) else {
        return libc::accept(fd, addr, len);
    };
    // The descriptor first: a connection taken off the queue must not be
    // lost to EMFILE.
    let (new_fd, ident) = match placeholder(libc::SOCK_STREAM) {
        Ok(p) => p,
        Err(e) => return set_errno(e),
    };
    let accepted = blocking(fd, Wait::Receive(listener), |s, pid| {
        let far = s.net.accept(listener)?;
        s.net.set_ident(far, ident);
        s.net.add_ref(pid, far);
        Ok(s.net.socks[far as usize].peer)
    });
    match accepted {
        Ok(peer) => {
            store_addr(&peer, addr, len);
            new_fd
        }
        Err(e) => {
            libc::close(new_fd);
            set_errno(e)
        }
    }
}

// ---- data -----------------------------------------------------------------

/// Not in the libc crate for Darwin; from `<sys/socket.h>`
const MSG_NOSIGNAL: c_int = 0x80000;

/// A write to a stream whose peer is gone raises SIGPIPE like the kernel
/// would, unless the socket or the call opted out.
fn broken_pipe(sock: u32, flags: c_int) -> isize {
    let quiet = flags & MSG_NOSIGNAL != 0
        || sched::with(|s, _| s.net.socks[sock as usize].nosigpipe) == Some(true);
    if !quiet {
        unsafe { libc::raise(libc::SIGPIPE) };
    }
    set_errno(libc::EPIPE) as isize
}

/// Where a datagram goes: the host and address `to` names, seen from the
/// sender's host.
fn dgram_dest(
    s: &shared::State,
    pid: u32,
    to: Option<Addr>,
) -> Result<Option<(u32, Addr)>, NetError> {
    let Some(to) = to else { return Ok(None) };
    let here = s.procs[pid as usize].host;
    if to.family != FAMILY_INET {
        return Ok(Some((here, to)));
    }
    match s.net.host_for_ip(here, to.ip) {
        Ok(h) => Ok(Some((h, to))),
        Err(true) => Err(NetError::Errno(libc::EHOSTUNREACH)),
        // An unconnected datagram socket cannot be half virtual
        Err(false) => Err(NetError::Errno(libc::ENETUNREACH)),
    }
}

/// Send on a virtual socket. A stream takes all of it unless the
/// descriptor is non-blocking; a datagram goes to `to` or to the default
/// destination.
pub fn send_to(
    fd: c_int,
    sock: u32,
    buf: *const u8,
    len: usize,
    flags: c_int,
    to: Option<Addr>,
) -> isize {
    let bytes = unsafe { std::slice::from_raw_parts(buf, len) };
    let wait = if flags & libc::MSG_DONTWAIT != 0 {
        Wait::Never
    } else {
        Wait::Send(sock)
    };
    if kind_of(sock) == KIND_DGRAM {
        let sent = blocking(fd, wait, |s, pid| {
            let dest = dgram_dest(s, pid, to)?;
            s.net.send_dgram(sock, dest, bytes)
        });
        return match sent {
            Ok(n) => n as isize,
            Err(e) => set_errno(e) as isize,
        };
    }
    let mut done = 0usize;
    loop {
        match blocking(fd, wait, |s, _| s.net.send(sock, &bytes[done..])) {
            Ok(n) => done += n,
            Err(_) if done > 0 => break,
            Err(libc::EPIPE) => return broken_pipe(sock, flags),
            Err(e) => return set_errno(e) as isize,
        }
        if done >= len {
            break;
        }
    }
    done as isize
}

pub fn send_fd(fd: c_int, sock: u32, buf: *const u8, len: usize, flags: c_int) -> isize {
    send_to(fd, sock, buf, len, flags, None)
}

/// Receive on a virtual socket; `from` gets a datagram's source.
pub fn recv_from(
    fd: c_int,
    sock: u32,
    buf: *mut u8,
    len: usize,
    flags: c_int,
    from: &mut Option<Addr>,
) -> isize {
    let out = unsafe { std::slice::from_raw_parts_mut(buf, len) };
    let wait = if flags & libc::MSG_DONTWAIT != 0 {
        Wait::Never
    } else {
        Wait::Receive(sock)
    };
    let peek = flags & libc::MSG_PEEK != 0;
    if kind_of(sock) == KIND_DGRAM {
        return match blocking(fd, wait, |s, _| s.net.recv_dgram(sock, out, peek)) {
            Ok((n, source)) => {
                *from = Some(source);
                n as isize
            }
            Err(e) => set_errno(e) as isize,
        };
    }
    let waitall = flags & libc::MSG_WAITALL != 0 && !peek;
    let mut done = 0usize;
    loop {
        match blocking(fd, wait, |s, _| s.net.recv(sock, &mut out[done..], peek)) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(_) if done > 0 => break,
            Err(e) => return set_errno(e) as isize,
        }
        if !waitall || done == len {
            break;
        }
    }
    done as isize
}

pub fn recv_fd(fd: c_int, sock: u32, buf: *mut u8, len: usize, flags: c_int) -> isize {
    recv_from(fd, sock, buf, len, flags, &mut None)
}

pub unsafe extern "C" fn my_send(fd: c_int, buf: *const c_void, n: usize, flags: c_int) -> isize {
    match lookup(fd) {
        Some(sock) => send_fd(fd, sock, buf.cast(), n, flags),
        None => libc::send(fd, buf, n, flags),
    }
}

pub unsafe extern "C" fn my_recv(fd: c_int, buf: *mut c_void, n: usize, flags: c_int) -> isize {
    match lookup(fd) {
        Some(sock) => recv_fd(fd, sock, buf.cast(), n, flags),
        None => libc::recv(fd, buf, n, flags),
    }
}

pub unsafe extern "C" fn my_sendto(
    fd: c_int,
    buf: *const c_void,
    n: usize,
    flags: c_int,
    addr: *const Sockaddr,
    len: Socklen,
) -> isize {
    match lookup(fd) {
        // A stream ignores the destination
        Some(sock) => send_to(fd, sock, buf.cast(), n, flags, parse_addr(addr, len)),
        None => libc::sendto(fd, buf, n, flags, addr, len),
    }
}

pub unsafe extern "C" fn my_recvfrom(
    fd: c_int,
    buf: *mut c_void,
    n: usize,
    flags: c_int,
    addr: *mut Sockaddr,
    len: *mut Socklen,
) -> isize {
    let Some(sock) = lookup(fd) else {
        return libc::recvfrom(fd, buf, n, flags, addr, len);
    };
    let mut from = None;
    let got = recv_from(fd, sock, buf.cast(), n, flags, &mut from);
    match from {
        Some(a) if got >= 0 => store_addr(&a, addr, len),
        _ if !len.is_null() => *len = 0,
        _ => {}
    }
    got
}

/// `sendmsg` without ancillary data: the vectors are gathered, since a
/// datagram must go out whole.
pub unsafe extern "C" fn my_sendmsg(fd: c_int, msg: *const libc::msghdr, flags: c_int) -> isize {
    let Some(sock) = lookup(fd) else {
        return libc::sendmsg(fd, msg, flags);
    };
    let m = &*msg;
    if m.msg_controllen > 0 {
        crate::report::log("sendmsg: ancillary data is not carried over virtual sockets");
    }
    let iov = std::slice::from_raw_parts(m.msg_iov, m.msg_iovlen.max(0) as usize);
    let mut data = Vec::new();
    for v in iov {
        data.extend_from_slice(std::slice::from_raw_parts(
            v.iov_base.cast::<u8>(),
            v.iov_len,
        ));
    }
    let to = parse_addr(m.msg_name.cast(), m.msg_namelen);
    send_to(fd, sock, data.as_ptr(), data.len(), flags, to)
}

pub unsafe extern "C" fn my_recvmsg(fd: c_int, msg: *mut libc::msghdr, flags: c_int) -> isize {
    let Some(sock) = lookup(fd) else {
        return libc::recvmsg(fd, msg, flags);
    };
    let m = &mut *msg;
    let iov = std::slice::from_raw_parts(m.msg_iov, m.msg_iovlen.max(0) as usize);
    let room: usize = iov.iter().map(|v| v.iov_len).sum();
    let mut data = vec![0u8; room.min(MAX_DGRAM.max(RING))];
    let mut from = None;
    let got = recv_from(fd, sock, data.as_mut_ptr(), data.len(), flags, &mut from);
    if got < 0 {
        return got;
    }
    let mut rest = &data[..got as usize];
    for v in iov {
        let n = rest.len().min(v.iov_len);
        std::ptr::copy_nonoverlapping(rest.as_ptr(), v.iov_base.cast::<u8>(), n);
        rest = &rest[n..];
    }
    match from {
        Some(a) if !m.msg_name.is_null() => {
            store_addr(&a, m.msg_name.cast(), &raw mut m.msg_namelen);
        }
        _ => m.msg_namelen = 0,
    }
    m.msg_controllen = 0;
    m.msg_flags = 0;
    got
}

pub unsafe extern "C" fn my_shutdown(fd: c_int, how: c_int) -> c_int {
    let Some(sock) = lookup(fd) else {
        return libc::shutdown(fd, how);
    };
    let (read, write) = (how != libc::SHUT_WR, how != libc::SHUT_RD);
    status(blocking(fd, Wait::Never, |s, _| {
        s.net.shutdown(sock, read, write)
    }))
}

// ---- names and options ----------------------------------------------------

pub unsafe extern "C" fn my_getsockname(
    fd: c_int,
    addr: *mut Sockaddr,
    len: *mut Socklen,
) -> c_int {
    let Some(sock) = lookup(fd) else {
        return libc::getsockname(fd, addr, len);
    };
    let Some(local) = sched::with(|s, _| {
        let k = &s.net.socks[sock as usize];
        let mut a = k.local;
        a.family = k.family;
        a
    }) else {
        return set_errno(libc::EBADF);
    };
    store_addr(&local, addr, len);
    0
}

pub unsafe extern "C" fn my_getpeername(
    fd: c_int,
    addr: *mut Sockaddr,
    len: *mut Socklen,
) -> c_int {
    let Some(sock) = lookup(fd) else {
        return libc::getpeername(fd, addr, len);
    };
    let peer = sched::with(|s, _| {
        let k = &s.net.socks[sock as usize];
        (k.far_end != shared::netstate::NO_SOCK).then_some(k.peer)
    })
    .flatten();
    match peer {
        Some(p) => {
            store_addr(&p, addr, len);
            0
        }
        None => set_errno(libc::ENOTCONN),
    }
}

const TCP_NODELAY: c_int = 0x01;
const TCP_KEEPALIVE: c_int = 0x10;
/// Private to XNU; libSystem's resolver sets it on the socket it opens to
/// mDNSResponder, before the connect that takes it out of the virtual
/// network.
const SO_DEFUNCTOK: c_int = 0x1100;

/// Options that mean nothing on the virtual network and are accepted so
/// that ordinary servers run unchanged
fn harmless(level: c_int, name: c_int) -> bool {
    match level {
        libc::SOL_SOCKET => matches!(
            name,
            libc::SO_REUSEADDR
                | libc::SO_REUSEPORT
                | libc::SO_KEEPALIVE
                | libc::SO_BROADCAST
                | libc::SO_LINGER
                | libc::SO_RCVBUF
                | libc::SO_SNDBUF
                | libc::SO_OOBINLINE
                | SO_DEFUNCTOK
        ),
        libc::IPPROTO_TCP => matches!(name, TCP_NODELAY | TCP_KEEPALIVE),
        _ => false,
    }
}

fn log_unknown_option(what: &str, level: c_int, name: c_int) {
    let mut line = String::new();
    let _ = std::fmt::Write::write_fmt(
        &mut line,
        format_args!("{what}: option {name:#x} at level {level:#x} is not modelled; ignored"),
    );
    crate::report::log(&line);
}

pub unsafe extern "C" fn my_setsockopt(
    fd: c_int,
    level: c_int,
    name: c_int,
    value: *const c_void,
    len: Socklen,
) -> c_int {
    let Some(sock) = lookup(fd) else {
        return libc::setsockopt(fd, level, name, value, len);
    };
    if level == libc::SOL_SOCKET && name == libc::SO_NOSIGPIPE {
        let on = !value.is_null()
            && len as usize >= std::mem::size_of::<c_int>()
            && value.cast::<c_int>().read_unaligned() != 0;
        sched::with(|s, _| s.net.socks[sock as usize].nosigpipe = on);
    } else if level == libc::SOL_SOCKET && (name == libc::SO_RCVTIMEO || name == libc::SO_SNDTIMEO)
    {
        if value.is_null() || (len as usize) < std::mem::size_of::<libc::timeval>() {
            return set_errno(libc::EINVAL);
        }
        let tv = value.cast::<libc::timeval>().read_unaligned();
        let ns = tv.tv_sec as u64 * 1_000_000_000 + tv.tv_usec as u64 * 1000;
        sched::with(|s, _| {
            let k = &mut s.net.socks[sock as usize];
            if name == libc::SO_RCVTIMEO {
                k.rcv_timeout_ns = ns;
            } else {
                k.snd_timeout_ns = ns;
            }
        });
    } else if !harmless(level, name) {
        log_unknown_option("setsockopt", level, name);
    }
    0
}

pub unsafe extern "C" fn my_getsockopt(
    fd: c_int,
    level: c_int,
    name: c_int,
    value: *mut c_void,
    len: *mut Socklen,
) -> c_int {
    let Some(sock) = lookup(fd) else {
        return libc::getsockopt(fd, level, name, value, len);
    };
    if value.is_null() || len.is_null() || (*len as usize) < std::mem::size_of::<c_int>() {
        return set_errno(libc::EINVAL);
    }
    let (kind, listening, nosigpipe) = sched::with(|s, _| {
        let k = &s.net.socks[sock as usize];
        (
            k.kind,
            k.state == shared::netstate::S_LISTENING,
            k.nosigpipe,
        )
    })
    .unwrap_or((KIND_STREAM, false, false));
    let v: c_int = match (level, name) {
        (libc::SOL_SOCKET, libc::SO_TYPE) if kind == KIND_DGRAM => libc::SOCK_DGRAM,
        (libc::SOL_SOCKET, libc::SO_TYPE) => libc::SOCK_STREAM,
        // Errors are reported where they happen; nothing is ever pending
        (libc::SOL_SOCKET, libc::SO_ERROR) => 0,
        (libc::SOL_SOCKET, libc::SO_ACCEPTCONN) => c_int::from(listening),
        (libc::SOL_SOCKET, libc::SO_NOSIGPIPE) => c_int::from(nosigpipe),
        (libc::SOL_SOCKET, libc::SO_RCVBUF | libc::SO_SNDBUF) => RING as c_int,
        _ => {
            if !harmless(level, name) {
                log_unknown_option("getsockopt", level, name);
            }
            0
        }
    };
    value.cast::<c_int>().write_unaligned(v);
    *len = std::mem::size_of::<c_int>() as Socklen;
    0
}

// ---- descriptors ----------------------------------------------------------

/// Called around a real `close`: `sock` is what `lookup` said before it.
pub fn closed(sock: u32) {
    sched::with(|s, pid| {
        s.net.drop_ref(pid, sock);
        s.wake_io();
    });
}

fn duplicated(sock: u32) {
    sched::with(|s, pid| s.net.add_ref(pid, sock));
}

pub unsafe extern "C" fn my_dup(fd: c_int) -> c_int {
    let sock = lookup(fd);
    let new = libc::dup(fd);
    if let (Some(sock), true) = (sock, new >= 0) {
        duplicated(sock);
    }
    new
}

pub unsafe extern "C" fn my_dup2(fd: c_int, target: c_int) -> c_int {
    let sock = lookup(fd);
    let replaced = if fd == target { None } else { lookup(target) };
    let new = libc::dup2(fd, target);
    if new >= 0 && fd != target {
        if let Some(sock) = sock {
            duplicated(sock);
        }
        if let Some(old) = replaced {
            closed(old);
        }
    }
    new
}

/// `fcntl` is variadic, and on arm64 Darwin variadic arguments travel on
/// the stack, which a Rust function cannot declare. The shim loads the
/// first one into x2. For commands without an argument it reads a word of
/// the caller's frame that nobody looks at.
#[no_mangle]
pub unsafe extern "C" fn rewrite_fcntl_impl(fd: c_int, cmd: c_int, arg: usize) -> c_int {
    let sock = if cmd == libc::F_DUPFD || cmd == libc::F_DUPFD_CLOEXEC {
        lookup(fd)
    } else {
        None
    };
    let rc = libc::fcntl(fd, cmd, arg);
    if let (Some(sock), true) = (sock, rc >= 0) {
        duplicated(sock);
    }
    rc
}

const FIONREAD: libc::c_ulong = 0x4004_667F;

/// `ioctl` is variadic like `fcntl`. Everything but `FIONREAD` works on
/// the placeholder as it is (`FIONBIO` sets its `O_NONBLOCK`).
#[no_mangle]
pub unsafe extern "C" fn rewrite_ioctl_impl(
    fd: c_int,
    request: libc::c_ulong,
    arg: usize,
) -> c_int {
    if request == FIONREAD {
        if let Some(sock) = lookup(fd) {
            let pending = sched::with(|s, _| s.net.pending_bytes(sock)).unwrap_or(0);
            (arg as *mut c_int).write_unaligned(pending as c_int);
            return 0;
        }
    }
    libc::ioctl(fd, request, arg)
}

extern "C" {
    pub fn rewrite_fcntl_shim();
    pub fn rewrite_ioctl_shim();
}

std::arch::global_asm!(
    ".globl _rewrite_fcntl_shim",
    ".p2align 2",
    "_rewrite_fcntl_shim:",
    "ldr x2, [sp]",
    "b _rewrite_fcntl_impl",
    ".globl _rewrite_ioctl_shim",
    ".p2align 2",
    "_rewrite_ioctl_shim:",
    "ldr x2, [sp]",
    "b _rewrite_ioctl_impl",
);

/// Count the virtual sockets among the descriptors this process was born
/// with (or kept across `execve`). The parent waits for this before it
/// goes on, so its own closes cannot make a shared socket look unused.
pub fn adopt_inherited(sh: &shared::Shared, pid: u32) {
    let limit = unsafe { libc::getdtablesize() }.clamp(0, 4096);
    let idents: Vec<u64> = (0..limit).filter_map(ident_of).collect();
    if idents.is_empty() {
        return;
    }
    let mut s = sh.lock();
    let held: Vec<u32> = idents.iter().filter_map(|&i| s.net.by_ident(i)).collect();
    s.net.set_refs(pid, &held);
}
