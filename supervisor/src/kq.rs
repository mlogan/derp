//! `kevent` for kqueues that watch virtual sockets. The kernel cannot see
//! those, so `EVFILT_READ` and `EVFILT_WRITE` registrations on them are
//! kept here, per kqueue, and their events are synthesized from socket
//! state. Every other registration stays on the real kqueue, which is
//! polled with a zero timeout alongside: the wait itself is the
//! scheduler's, in virtual time, because what ends it (socket traffic, a
//! user event another thread triggers) comes from scheduled threads. Only a
//! kqueue whose every registration belongs to the outside world waits in
//! the kernel. Level-triggered, `EV_CLEAR` and
//! `EV_ONESHOT` registrations and `EV_RECEIPT` are modelled; `EV_DISPATCH` and
//! the rest are logged and treated as level-triggered.
//!
//! A kqueue is not inherited by `fork`, so the child starts with an empty
//! registry (`forked`).

use std::ffi::{c_int, c_void};

use crate::sched::{self, my_id};
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
    /// Every descriptor of this kqueue: a runtime may register through one
    /// duplicate and wait on another
    fds: Vec<c_int>,
    regs: Vec<Reg>,
    /// Pipes and kernel sockets between guests registered on the real
    /// kqueue: their readiness also only changes when a guest acts
    guest_objects: Vec<c_int>,
    /// Registrations only the world outside the run can fire (a descriptor
    /// that is no guest's, a signal, a kernel timer), as (ident, filter)
    external: Vec<(usize, i16)>,
}

struct Registry(Vec<Kq>);

// The raw `udata` pointers are only handed back to the guest.
unsafe impl Send for Registry {}

static KQS: SpinLock<Registry> = SpinLock::new(Registry(Vec::new()));

/// In the child of a `fork`: the parent's kqueues are not ours.
pub fn forked() {
    KQS.force_unlock();
    KQS.lock().0.clear();
}

/// A descriptor was closed: a kqueue goes away with its registrations, and
/// any other descriptor drops out of every kqueue, as in the kernel.
pub fn closed(fd: c_int) {
    let mut kqs = KQS.lock();
    for k in &mut kqs.0 {
        k.fds.retain(|&f| f != fd);
    }
    kqs.0.retain(|k| !k.fds.is_empty());
    for k in &mut kqs.0 {
        k.external
            .retain(|&(ident, filter)| ident != fd as usize || filter == libc::EVFILT_SIGNAL);
        k.regs.retain(|r| r.fd != fd);
        k.guest_objects.retain(|&o| o != fd);
    }
}

/// `new` is a duplicate of `fd`.
pub fn duplicated(fd: c_int, new: c_int) {
    let mut kqs = KQS.lock();
    if let Some(k) = kqs.0.iter_mut().find(|k| k.fds.contains(&fd)) {
        k.fds.push(new);
    }
}

pub unsafe extern "C" fn my_kqueue() -> c_int {
    let fd = libc::kqueue();
    if fd >= 0 && my_id().is_some() {
        // Known from birth, so that duplicates made before its first
        // `kevent` are known too
        KQS.lock().0.push(Kq::new(fd));
    }
    fd
}

impl Kq {
    fn new(fd: c_int) -> Kq {
        Kq {
            fds: vec![fd],
            regs: Vec::new(),
            guest_objects: Vec::new(),
            external: Vec::new(),
        }
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

const UNMODELLED: u16 = libc::EV_DISPATCH;

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
        crate::report::log("kevent: EV_DISPATCH is not modelled on virtual sockets");
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
    sched::hook_event(sched::SITE_WAIT);
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
    // What the kernel would answer to our share of `EV_RECEIPT` changes
    let mut receipts: Vec<libc::kevent> = Vec::new();
    let wants_receipts = changes.iter().any(|ev| ev.flags & libc::EV_RECEIPT != 0);
    let ours = {
        let mut kqs = KQS.lock();
        let known = kqs.0.iter().position(|k| k.fds.contains(&kq));
        let at = known.unwrap_or_else(|| {
            kqs.0.push(Kq::new(kq));
            kqs.0.len() - 1
        });
        let k = &mut kqs.0[at];
        for ev in changes {
            let io = ev.filter == libc::EVFILT_READ || ev.filter == libc::EVFILT_WRITE;
            let fd = ev.ident as c_int;
            match crate::net::lookup(fd) {
                Some(sock) if io => {
                    change(k, ev, sock);
                    if ev.flags & libc::EV_RECEIPT != 0 {
                        receipts.push(libc::kevent {
                            flags: ev.flags | libc::EV_ERROR,
                            data: 0,
                            ..*ev
                        });
                    }
                }
                _ => {
                    let guest_object = io && crate::io::is_guest_object(fd);
                    if guest_object && !k.guest_objects.contains(&fd) {
                        k.guest_objects.push(fd);
                    }
                    // A user event is triggered by a `kevent` call, which
                    // in a run only a scheduled thread makes
                    if !guest_object && ev.filter != libc::EVFILT_USER {
                        let key = (ev.ident, ev.filter);
                        k.external.retain(|&e| e != key);
                        if ev.flags & libc::EV_DELETE == 0 {
                            k.external.push(key);
                        }
                    }
                    real_changes.push(*ev);
                }
            }
        }
        !k.regs.is_empty() || !k.guest_objects.is_empty() || k.external.is_empty()
    };
    if !ours && !wants_receipts {
        // Only the outside world can end this wait: a real wait is right
        return libc::kevent(kq, changes.as_ptr(), nchanges, events, nevents, timeout);
    }
    let zero = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if wants_receipts {
        // A call with receipts reports on its changes and never waits
        let room = if events.is_null() {
            0
        } else {
            nevents.max(0) as usize
        };
        let out = std::slice::from_raw_parts_mut(events, room);
        let mut n = 0;
        if !real_changes.is_empty() {
            let rc = libc::kevent(
                kq,
                real_changes.as_ptr(),
                real_changes.len() as c_int,
                out.as_mut_ptr(),
                room as c_int,
                &raw const zero,
            );
            if rc < 0 {
                return rc;
            }
            n = rc as usize;
            crate::io::wake_io();
        }
        for r in receipts {
            if n < room {
                out[n] = r;
                n += 1;
            }
        }
        return n as c_int;
    }
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
        crate::io::wake_io();
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
            match kqs.0.iter_mut().find(|k| k.fds.contains(&kq)) {
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
        if crate::io::park_for_io(deadline) {
            return 0;
        }
    }
}
