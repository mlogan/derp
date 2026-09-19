//! `poll` and `select` over a mix of virtual sockets and kernel
//! descriptors. Readiness of a virtual socket comes from the shared state,
//! of the rest from a real `poll` with a zero timeout. If nothing is ready
//! the thread parks as an I/O waiter with the timeout as a virtual-time
//! deadline and looks again when woken.

use std::ffi::c_int;

use crate::sched::{self, my_id};
use crate::shared;
use crate::shared::netstate::{KIND_DGRAM, S_CLOSED, S_CONNECTED};

/// Events a virtual socket has now, limited to what was asked for (plus
/// the ones `poll` always reports).
fn virtual_events(sock: u32, wanted: libc::c_short) -> libc::c_short {
    sched::with(|s, _| {
        let k = &s.net.socks[sock as usize];
        let mut ev = 0;
        if wanted & (libc::POLLIN | libc::POLLRDNORM) != 0 && s.net.readable(sock) {
            ev |= wanted & (libc::POLLIN | libc::POLLRDNORM);
        }
        if wanted & (libc::POLLOUT | libc::POLLWRNORM) != 0 && s.net.writable(sock) {
            ev |= wanted & (libc::POLLOUT | libc::POLLWRNORM);
        }
        // Both directions are over: the peer sent FIN and is gone
        if k.kind != KIND_DGRAM && k.state == S_CONNECTED && k.fin {
            let peer = &s.net.socks[k.far_end as usize];
            if peer.state == S_CLOSED {
                ev |= libc::POLLHUP;
            }
        }
        ev
    })
    .unwrap_or(libc::POLLNVAL)
}

/// One pass over the set; returns how many entries have events.
unsafe fn scan(fds: &mut [libc::pollfd], socks: &[Option<u32>]) -> c_int {
    let mut real: Vec<libc::pollfd> = Vec::new();
    for (p, sock) in fds.iter_mut().zip(socks) {
        p.revents = 0;
        match sock {
            Some(sock) => p.revents = virtual_events(*sock, p.events),
            None if p.fd >= 0 => real.push(*p),
            None => {}
        }
    }
    if !real.is_empty() {
        libc::poll(real.as_mut_ptr(), real.len() as libc::nfds_t, 0);
        let mut answers = real.iter();
        for (p, sock) in fds.iter_mut().zip(socks) {
            if sock.is_none() && p.fd >= 0 {
                p.revents = answers.next().map_or(0, |r| r.revents);
            }
        }
    }
    fds.iter().filter(|p| p.revents != 0).count() as c_int
}

/// Wait for readiness in the scheduler. `timeout_ns` of `None` is forever.
unsafe fn wait(fds: &mut [libc::pollfd], timeout_ns: Option<u64>) -> c_int {
    let socks: Vec<Option<u32>> = fds.iter().map(|p| crate::net::lookup(p.fd)).collect();
    let deadline = timeout_ns.map(|t| sched::now().saturating_add(t));
    loop {
        let ready = scan(fds, &socks);
        if ready != 0 || timeout_ns == Some(0) {
            return ready;
        }
        crate::io::IO_WAITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if sched::block_until(shared::IO_KEY, deadline) {
            return scan(fds, &socks);
        }
    }
}

/// A block may end early without a wake (see `ProcRec::outside_wakes`).
fn sleep_until(deadline: u64) {
    while !sched::block_until(shared::SLEEP_KEY, Some(deadline)) {}
}

/// Whether the set is ours to wait on: some descriptor's readiness must
/// depend on another guest. A set of only ttys, files or the launcher's
/// own pipes is waited on for real.
unsafe fn between_guests(fds: &[libc::pollfd]) -> bool {
    my_id().is_some()
        && fds.iter().any(|p| {
            p.fd >= 0 && (crate::net::lookup(p.fd).is_some() || crate::io::is_guest_object(p.fd))
        })
}

pub unsafe extern "C" fn my_poll(fds: *mut libc::pollfd, n: libc::nfds_t, timeout: c_int) -> c_int {
    sched::hook_event(sched::SITE_WAIT);
    let set = if fds.is_null() {
        &mut [][..]
    } else {
        std::slice::from_raw_parts_mut(fds, n as usize)
    };
    if my_id().is_some() && set.is_empty() && timeout > 0 {
        // A sleep in disguise
        sleep_until(sched::now().saturating_add(timeout as u64 * 1_000_000));
        return 0;
    }
    if !between_guests(set) {
        return libc::poll(fds, n, timeout);
    }
    let timeout_ns = (timeout >= 0).then(|| timeout as u64 * 1_000_000);
    wait(set, timeout_ns)
}

const FD_SETSIZE: usize = 1024;

unsafe fn is_set(set: *const libc::fd_set, fd: usize) -> bool {
    !set.is_null() && libc::FD_ISSET(fd as c_int, set)
}

pub unsafe extern "C" fn my_select(
    nfds: c_int,
    readfds: *mut libc::fd_set,
    writefds: *mut libc::fd_set,
    errorfds: *mut libc::fd_set,
    timeout: *mut libc::timeval,
) -> c_int {
    sched::hook_event(sched::SITE_WAIT);
    let n = (nfds.max(0) as usize).min(FD_SETSIZE);
    let mut fds: Vec<libc::pollfd> = Vec::new();
    for fd in 0..n {
        let mut events = 0;
        if is_set(readfds, fd) {
            events |= libc::POLLIN;
        }
        if is_set(writefds, fd) {
            events |= libc::POLLOUT;
        }
        if is_set(errorfds, fd) {
            events |= libc::POLLPRI;
        }
        if events != 0 {
            fds.push(libc::pollfd {
                fd: fd as c_int,
                events,
                revents: 0,
            });
        }
    }
    let timeout_ns = (!timeout.is_null())
        .then(|| (*timeout).tv_sec as u64 * 1_000_000_000 + (*timeout).tv_usec as u64 * 1000);
    if my_id().is_some() && fds.is_empty() {
        if let Some(ns) = timeout_ns.filter(|&ns| ns > 0) {
            sleep_until(sched::now().saturating_add(ns));
            return 0;
        }
    }
    if !between_guests(&fds) {
        return libc::select(nfds, readfds, writefds, errorfds, timeout);
    }
    wait(&mut fds, timeout_ns);
    if fds.iter().any(|p| p.revents & libc::POLLNVAL != 0) {
        *libc::__error() = libc::EBADF;
        return -1;
    }
    for set in [readfds, writefds, errorfds] {
        if !set.is_null() {
            libc::FD_ZERO(set);
        }
    }
    let mut count = 0;
    for p in &fds {
        let hup = p.revents & (libc::POLLHUP | libc::POLLERR) != 0;
        if !readfds.is_null()
            && p.events & libc::POLLIN != 0
            && (p.revents & libc::POLLIN != 0 || hup)
        {
            libc::FD_SET(p.fd, readfds);
            count += 1;
        }
        if !writefds.is_null() && p.events & libc::POLLOUT != 0 && p.revents & libc::POLLOUT != 0 {
            libc::FD_SET(p.fd, writefds);
            count += 1;
        }
        if !errorfds.is_null() && p.events & libc::POLLPRI != 0 && p.revents & libc::POLLPRI != 0 {
            libc::FD_SET(p.fd, errorfds);
            count += 1;
        }
    }
    count
}
