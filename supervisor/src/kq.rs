//! `kevent` for kqueues that watch virtual sockets. The kernel cannot see
//! those, so `EVFILT_READ` and `EVFILT_WRITE` registrations on them are
//! kept here, per kqueue, and their events are synthesized from socket
//! state. Every other registration stays on the real kqueue, which is
//! polled with a zero timeout alongside. Level-triggered, `EV_CLEAR` and
//! `EV_ONESHOT` registrations are modelled; `EV_DISPATCH` and the rest are
//! logged and treated as level-triggered.
//!
//! The registry is process-local: a kqueue is not inherited by `fork`.

use std::ffi::{c_int, c_void};

use crate::sched::{self, my_id};
use crate::shared;
use crate::shared::netstate::{KIND_DGRAM, S_CONNECTED, S_LISTENING};
use crate::spin::SpinLock;

struct Reg {
    fd: c_int,
    sock: u32,
    filter: i16,
    flags: u16,
    udata: *mut c_void,
    enabled: bool,
    /// The socket's event counter when this last fired, for `EV_CLEAR`
    seen: Option<u64>,
}

struct Kq {
    fd: c_int,
    regs: Vec<Reg>,
    /// Pipes and kernel sockets between guests registered on the real
    /// kqueue: their readiness also only changes when a guest acts
    guest_objects: Vec<c_int>,
}

struct Registry(Vec<Kq>);

// The raw `udata` pointers are only handed back to the guest.
unsafe impl Send for Registry {}

static KQS: SpinLock<Registry> = SpinLock::new(Registry(Vec::new()));

/// A descriptor was closed: a kqueue goes away with its registrations, and
/// any other descriptor drops out of every kqueue, as in the kernel.
pub fn closed(fd: c_int) {
    let mut kqs = KQS.lock();
    kqs.0.retain(|k| k.fd != fd);
    for k in &mut kqs.0 {
        k.regs.retain(|r| r.fd != fd);
        k.guest_objects.retain(|&o| o != fd);
    }
}

struct Snapshot {
    ready: bool,
    eof: bool,
    data: isize,
    events: u64,
}

fn snapshot(sock: u32, filter: i16) -> Option<Snapshot> {
    sched::with(|s, _| {
        let k = &s.net.socks[sock as usize];
        if filter == libc::EVFILT_READ {
            let data = if k.state == S_LISTENING {
                // `readable` is true exactly when the queue is not empty
                isize::from(s.net.readable(sock))
            } else {
                s.net.pending_bytes(sock) as isize
            };
            Snapshot {
                ready: s.net.readable(sock),
                eof: k.kind != KIND_DGRAM && k.state == S_CONNECTED && k.fin,
                data,
                events: k.rd_events,
            }
        } else {
            Snapshot {
                ready: s.net.writable(sock),
                eof: false,
                data: 0,
                events: k.wr_events,
            }
        }
    })
}

const UNMODELLED: u16 = libc::EV_DISPATCH | libc::EV_RECEIPT;

/// Apply one change to a virtual registration.
fn change(kq: &mut Kq, ev: &libc::kevent, sock: u32) {
    let fd = ev.ident as c_int;
    let at = kq
        .regs
        .iter()
        .position(|r| r.fd == fd && r.filter == ev.filter);
    if ev.flags & libc::EV_DELETE != 0 {
        if let Some(at) = at {
            kq.regs.remove(at);
        }
        return;
    }
    if ev.flags & UNMODELLED != 0 {
        crate::report::log(
            "kevent: EV_DISPATCH and EV_RECEIPT are not modelled on virtual sockets",
        );
    }
    let reg = match at {
        Some(at) => &mut kq.regs[at],
        None if ev.flags & libc::EV_ADD != 0 => {
            kq.regs.push(Reg {
                fd,
                sock,
                filter: ev.filter,
                flags: 0,
                udata: std::ptr::null_mut(),
                enabled: true,
                seen: None,
            });
            kq.regs.last_mut().unwrap()
        }
        None => return,
    };
    if ev.flags & libc::EV_ADD != 0 {
        reg.flags = ev.flags & (libc::EV_CLEAR | libc::EV_ONESHOT);
        reg.udata = ev.udata;
        reg.sock = sock;
        // Re-adding re-arms an edge-triggered registration
        reg.seen = None;
    }
    if ev.flags & libc::EV_ENABLE != 0 {
        reg.enabled = true;
    }
    if ev.flags & libc::EV_DISABLE != 0 {
        reg.enabled = false;
    }
}

/// Events the virtual registrations have now, up to `out.len()`.
fn collect(kq: &mut Kq, out: &mut [libc::kevent]) -> usize {
    let mut n = 0;
    let mut fired_oneshot = Vec::new();
    for (i, r) in kq.regs.iter_mut().enumerate() {
        if n == out.len() {
            break;
        }
        if !r.enabled {
            continue;
        }
        let Some(snap) = snapshot(r.sock, r.filter) else {
            continue;
        };
        let edge = r.flags & libc::EV_CLEAR != 0;
        if !snap.ready || edge && r.seen == Some(snap.events) {
            continue;
        }
        r.seen = Some(snap.events);
        let mut flags = r.flags | libc::EV_ADD;
        if snap.eof {
            flags |= libc::EV_EOF;
        }
        out[n] = libc::kevent {
            ident: r.fd as usize,
            filter: r.filter,
            flags,
            fflags: 0,
            data: snap.data,
            udata: r.udata,
        };
        n += 1;
        if r.flags & libc::EV_ONESHOT != 0 {
            fired_oneshot.push(i);
        }
    }
    for i in fired_oneshot.into_iter().rev() {
        kq.regs.remove(i);
    }
    n
}

pub unsafe extern "C" fn my_kevent(
    kq: c_int,
    changes: *const libc::kevent,
    nchanges: c_int,
    events: *mut libc::kevent,
    nevents: c_int,
    timeout: *const libc::timespec,
) -> c_int {
    if my_id().is_none() {
        return libc::kevent(kq, changes, nchanges, events, nevents, timeout);
    }
    let changes = if changes.is_null() {
        &[][..]
    } else {
        std::slice::from_raw_parts(changes, nchanges.max(0) as usize)
    };
    // Split the changes: ours, and the kernel's
    let mut real_changes: Vec<libc::kevent> = Vec::new();
    let ours = {
        let mut kqs = KQS.lock();
        let at = kqs.0.iter().position(|k| k.fd == kq).unwrap_or_else(|| {
            kqs.0.push(Kq {
                fd: kq,
                regs: Vec::new(),
                guest_objects: Vec::new(),
            });
            kqs.0.len() - 1
        });
        let k = &mut kqs.0[at];
        for ev in changes {
            let io = ev.filter == libc::EVFILT_READ || ev.filter == libc::EVFILT_WRITE;
            let fd = ev.ident as c_int;
            match crate::net::lookup(fd) {
                Some(sock) if io => change(k, ev, sock),
                _ => {
                    if io && crate::io::is_guest_object(fd) && !k.guest_objects.contains(&fd) {
                        k.guest_objects.push(fd);
                    }
                    real_changes.push(*ev);
                }
            }
        }
        !k.regs.is_empty() || !k.guest_objects.is_empty()
    };
    if !ours {
        // Nothing here depends on another guest: a real wait is right
        return libc::kevent(kq, changes.as_ptr(), nchanges, events, nevents, timeout);
    }
    let zero = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if !real_changes.is_empty() {
        let rc = libc::kevent(
            kq,
            real_changes.as_ptr(),
            real_changes.len() as c_int,
            std::ptr::null_mut(),
            0,
            &raw const zero,
        );
        if rc < 0 {
            return rc;
        }
        // A change (a user event triggered, say) may be what a waiter on
        // this kqueue in another thread is waiting for
        sched::with(|s, _| s.wake_io());
    }
    if events.is_null() || nevents <= 0 {
        return 0;
    }
    let out = std::slice::from_raw_parts_mut(events, nevents as usize);
    let timeout_ns = (!timeout.is_null())
        .then(|| (*timeout).tv_sec as u64 * 1_000_000_000 + (*timeout).tv_nsec as u64);
    let deadline = timeout_ns.map(|t| sched::now() + t);
    loop {
        let mut n = {
            let mut kqs = KQS.lock();
            match kqs.0.iter_mut().find(|k| k.fd == kq) {
                Some(k) => collect(k, out),
                None => 0,
            }
        };
        if n < out.len() {
            let got = libc::kevent(
                kq,
                std::ptr::null(),
                0,
                out[n..].as_mut_ptr(),
                (out.len() - n) as c_int,
                &raw const zero,
            );
            if got > 0 {
                n += got as usize;
            }
        }
        if n > 0 || timeout_ns == Some(0) {
            return n as c_int;
        }
        crate::io::IO_WAITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if sched::block_until(shared::IO_KEY, deadline) {
            return 0;
        }
    }
}
