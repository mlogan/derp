//! `SIGCHLD` as a point of the schedule. The kernel sends it when a child
//! really dies, to whichever thread it likes, at a moment of real time; a
//! handler that wakes someone (every async runtime's does) would make the
//! run depend on that moment. The run already knows of the death at a
//! fixed point, so the guest's handler is kept here, the kernel's delivery
//! is dropped, and the handler runs on the parent's next thread to take up
//! the baton.

use std::ffi::c_int;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::spin::SpinLock;

struct Kept(Option<libc::sigaction>);

// The handler address is only ever called, on a thread of this process.
unsafe impl Send for Kept {}

static GUEST: SpinLock<Kept> = SpinLock::new(Kept(None));

/// The thread our own delivery is for; the kernel's go to the others, or
/// come when this is clear
static DELIVERING_TO: AtomicUsize = AtomicUsize::new(0);

/// In the child of a `fork`: see `SpinLock::force_unlock`.
pub fn forked() {
    GUEST.force_unlock();
}

extern "C" fn on_sigchld(sig: c_int, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
    let me = unsafe { libc::pthread_self() } as usize;
    if DELIVERING_TO.load(Ordering::Relaxed) != me {
        return;
    }
    let Some(action) = GUEST.lock().0 else { return };
    let handler = action.sa_sigaction;
    unsafe {
        if action.sa_flags & libc::SA_SIGINFO != 0 {
            let f: extern "C" fn(c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                std::mem::transmute(handler);
            f(sig, info, context);
        } else {
            let f: extern "C" fn(c_int) = std::mem::transmute(handler);
            f(sig);
        }
    }
}

/// A child of this process died at this point of the schedule: run the
/// guest's handler here, on the thread that holds the baton.
pub fn deliver_sigchld() {
    if GUEST.lock().0.is_none() {
        return;
    }
    let me = unsafe { libc::pthread_self() };
    DELIVERING_TO.store(me as usize, Ordering::Relaxed);
    // Synchronous for a signal a thread sends itself, unless it blocks it;
    // then it stays pending on this thread and is dropped when it comes
    unsafe { libc::pthread_kill(me, libc::SIGCHLD) };
    DELIVERING_TO.store(0, Ordering::Relaxed);
}

pub unsafe extern "C" fn my_sigaction(
    sig: c_int,
    new: *const libc::sigaction,
    old: *mut libc::sigaction,
) -> c_int {
    if sig != libc::SIGCHLD || !crate::coord::connected() {
        return libc::sigaction(sig, new, old);
    }
    let mut kept = GUEST.lock();
    if !old.is_null() {
        if let Some(action) = kept.0 {
            *old = action;
        } else {
            let rc = libc::sigaction(sig, std::ptr::null(), old);
            if rc != 0 {
                return rc;
            }
        }
    }
    if new.is_null() {
        return 0;
    }
    let handler = (*new).sa_sigaction;
    if handler == libc::SIG_DFL || handler == libc::SIG_IGN {
        // Nothing of the guest's would run: the kernel's meaning stands
        // (ignoring SIGCHLD also means its children are reaped for it)
        let rc = libc::sigaction(sig, new, std::ptr::null_mut());
        if rc == 0 {
            kept.0 = None;
        }
        return rc;
    }
    let mut ours = *new;
    ours.sa_sigaction = on_sigchld as *const () as usize;
    ours.sa_flags |= libc::SA_SIGINFO;
    let rc = libc::sigaction(sig, &raw const ours, std::ptr::null_mut());
    if rc == 0 {
        kept.0 = Some(*new);
    }
    rc
}

pub unsafe extern "C" fn my_signal(sig: c_int, handler: libc::sighandler_t) -> libc::sighandler_t {
    if sig != libc::SIGCHLD || !crate::coord::connected() {
        return libc::signal(sig, handler);
    }
    let mut new: libc::sigaction = std::mem::zeroed();
    new.sa_sigaction = handler;
    new.sa_flags = libc::SA_RESTART;
    let mut old: libc::sigaction = std::mem::zeroed();
    if my_sigaction(sig, &raw const new, &raw mut old) != 0 {
        return libc::SIG_ERR;
    }
    old.sa_sigaction
}
