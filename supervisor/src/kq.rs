//! `kevent` for kqueues that watch virtual sockets. The kernel cannot see
//! those, so `EVFILT_READ` and `EVFILT_WRITE` registrations on them are
//! kept here, per kqueue, and their events are synthesized from socket
//! state. Every other registration stays on the real kqueue, which is
//! polled with a zero timeout alongside: the wait itself is the
//! scheduler's, in virtual time, because what ends it (socket traffic, a
//! user event another thread triggers) comes from scheduled threads. Only a
//! kqueue whose every registration belongs to the outside world waits in
//! the kernel. Level-triggered, `EV_CLEAR`, `EV_ONESHOT` and `EV_DISPATCH`
//! registrations are modelled, and `EV_RECEIPT`.
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
    /// Idents of the user events registered: a scheduled thread's `kevent`
    /// triggers them, so a wait on them is the scheduler's
    user_events: Vec<usize>,
}

/// A change of a call that wants receipts
enum Step {
    /// A virtual registration's, with the receipt the kernel would give
    Ours(libc::kevent),
    Kernels(libc::kevent),
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
        // Only these filters' idents are descriptors; a timer's or a
        // process's may equal `fd` by chance
        let of_a_descriptor =
            |f: i16| [libc::EVFILT_READ, libc::EVFILT_WRITE, libc::EVFILT_VNODE].contains(&f);
        k.external
            .retain(|&(ident, filter)| ident != fd as usize || !of_a_descriptor(filter));
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
    if fd >= 0 {
        // A stale entry under this number, closed where we did not see it
        closed(fd);
    }
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
            user_events: Vec::new(),
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
        reg.flags = ev.flags & (libc::EV_CLEAR | libc::EV_ONESHOT | libc::EV_DISPATCH);
        reg.udata = ev.udata;
        reg.sock = sock;
        // Re-adding re-arms an edge-triggered registration
        reg.seen = None;
    }
    if ev.flags & libc::EV_ENABLE != 0 {
        reg.enabled = true;
        // The kernel looks again on enable: unread data fires once more
        if reg.flags & libc::EV_DISPATCH != 0 {
            reg.seen = None;
        }
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
        } else if r.flags & libc::EV_DISPATCH != 0 {
            // Delivered once, then silent until the guest enables it again
            r.enabled = false;
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
    // The changes in the order given, for a call that wants receipts
    let mut ordered: Vec<Step> = Vec::new();
    let wants_receipts = changes.iter().any(|ev| ev.flags & libc::EV_RECEIPT != 0);
    let (ours, outside) = {
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
                        ordered.push(Step::Ours(libc::kevent {
                            flags: ev.flags | libc::EV_ERROR,
                            data: 0,
                            ..*ev
                        }));
                    }
                }
                _ => {
                    let guest_object = io && crate::io::is_guest_object(fd);
                    if guest_object && !k.guest_objects.contains(&fd) {
                        k.guest_objects.push(fd);
                    }
                    // A user event is triggered by a `kevent` call, which
                    // in a run only a scheduled thread makes
                    let deleted = ev.flags & libc::EV_DELETE != 0;
                    if ev.filter == libc::EVFILT_USER {
                        k.user_events.retain(|&i| i != ev.ident);
                        if !deleted {
                            k.user_events.push(ev.ident);
                        }
                    } else if !guest_object {
                        let key = (ev.ident, ev.filter);
                        k.external.retain(|&e| e != key);
                        if !deleted {
                            k.external.push(key);
                        }
                    }
                    real_changes.push(*ev);
                    ordered.push(Step::Kernels(*ev));
                }
            }
        }
        let ours = !k.regs.is_empty()
            || !k.guest_objects.is_empty()
            || !k.user_events.is_empty()
            || k.external.is_empty();
        (ours, !k.external.is_empty())
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
        // A call with receipts reports on its changes, in their order, and
        // never waits. The kernel's are asked for one change at a time, with
        // room for that receipt alone, so that no pending event comes back
        // in a receipt's place.
        let room = if events.is_null() {
            0
        } else {
            nevents.max(0) as usize
        };
        let out: &mut [libc::kevent] = if room == 0 {
            &mut []
        } else {
            std::slice::from_raw_parts_mut(events, room)
        };
        let mut n = 0;
        for step in ordered {
            match step {
                Step::Ours(receipt) if n < room => {
                    out[n] = receipt;
                    n += 1;
                }
                Step::Ours(_) => {}
                Step::Kernels(ev) => {
                    let wants = ev.flags & libc::EV_RECEIPT != 0 && n < room;
                    let rc = libc::kevent(
                        kq,
                        &raw const ev,
                        1,
                        out[n..].as_mut_ptr(),
                        c_int::from(wants),
                        &raw const zero,
                    );
                    if rc < 0 {
                        return rc;
                    }
                    n += rc as usize;
                }
            }
        }
        if !real_changes.is_empty() {
            crate::io::wake_io();
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
    let timeout_ns = (!timeout.is_null()).then(|| sched::timespec_ns(timeout));
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
                // A one-shot registration that fired is gone from the kernel
                let fired = &out[n..n + got as usize];
                if fired.iter().any(|e| e.flags & libc::EV_ONESHOT != 0) {
                    let mut kqs = KQS.lock();
                    if let Some(k) = kqs.0.iter_mut().find(|k| k.fds.contains(&kq)) {
                        for e in fired.iter().filter(|e| e.flags & libc::EV_ONESHOT != 0) {
                            k.external.retain(|&x| x != (e.ident, e.filter));
                        }
                    }
                }
                n += got as usize;
            }
        }
        if n > 0 || timeout_ns == Some(0) {
            return n as c_int;
        }
        if outside {
            crate::io::note_outside_in_wait();
        }
        if crate::io::park_for_io(deadline) {
            return 0;
        }
    }
}
