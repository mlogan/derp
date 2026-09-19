//! Grand Central Dispatch is not supported in guests, and says so. Its
//! worker threads are made by the kernel (not `pthread_create`), so the
//! scheduler never runs them: work a guest put on a dispatch queue would
//! happen outside the baton, in real time, and the run would stop being a
//! function of the seed without any sign of it. A guest that submits work
//! to GCD therefore exits with an error naming the call.
//!
//! Only the guest's own code is held to this. System libraries use GCD
//! internally all the time (CoreFoundation reads preferences with
//! `dispatch_apply` while curl starts); what they do there is input, like
//! the contents of a file. The caller's return address tells the two
//! apart. `dispatch_sync` and the semaphore calls stay available: they run
//! on the calling thread or are scheduled waits.

use crate::process::in_system_library;

/// Exit status of a guest that used an unsupported facility (`EX_UNAVAILABLE`)
const EX_UNAVAILABLE: i32 = 69;

/// Called by every shim with the return address of the guest's call.
#[no_mangle]
pub extern "C" fn rewrite_gcd_check(caller: usize, which: usize) {
    if !crate::coord::connected() || in_system_library(caller) {
        return;
    }
    let mut line = String::from(NAMES.get(which).copied().unwrap_or("dispatch"));
    line.push_str(
        ": Grand Central Dispatch is not supported; its worker threads run outside the scheduler",
    );
    crate::report::log(&line);
    unsafe { libc::exit(EX_UNAVAILABLE) };
}

/// One shim per refused entry point. A shim keeps the argument registers,
/// asks `rewrite_gcd_check` about the caller, and continues into the real
/// function (a branch from this image is not interposed) as if nothing had
/// happened. None of these functions takes floating-point arguments.
macro_rules! refused {
    ($($index:literal $name:ident $shim:ident),* $(,)?) => {
        const NAMES: &[&str] = &[$(stringify!($name)),*];
        extern "C" {
            $(pub fn $name(); pub fn $shim();)*
        }
        $(std::arch::global_asm!(
            concat!(".globl _", stringify!($shim)),
            ".p2align 2",
            concat!("_", stringify!($shim), ":"),
            "stp x29, x30, [sp, #-16]!",
            "stp x0, x1, [sp, #-16]!",
            "stp x2, x3, [sp, #-16]!",
            "stp x4, x5, [sp, #-16]!",
            "stp x6, x7, [sp, #-16]!",
            "mov x0, x30",
            concat!("mov x1, #", $index),
            "bl _rewrite_gcd_check",
            "ldp x6, x7, [sp], #16",
            "ldp x4, x5, [sp], #16",
            "ldp x2, x3, [sp], #16",
            "ldp x0, x1, [sp], #16",
            "ldp x29, x30, [sp], #16",
            concat!("b _", stringify!($name)),
        );)*
    };
}

refused! {
    0 dispatch_async rewrite_dispatch_async_shim,
    1 dispatch_async_f rewrite_dispatch_async_f_shim,
    2 dispatch_after rewrite_dispatch_after_shim,
    3 dispatch_after_f rewrite_dispatch_after_f_shim,
    4 dispatch_apply rewrite_dispatch_apply_shim,
    5 dispatch_apply_f rewrite_dispatch_apply_f_shim,
    6 dispatch_group_async rewrite_dispatch_group_async_shim,
    7 dispatch_group_async_f rewrite_dispatch_group_async_f_shim,
    8 dispatch_barrier_async rewrite_dispatch_barrier_async_shim,
    9 dispatch_barrier_async_f rewrite_dispatch_barrier_async_f_shim,
    10 dispatch_group_notify rewrite_dispatch_group_notify_shim,
    11 dispatch_group_notify_f rewrite_dispatch_group_notify_f_shim,
    12 dispatch_source_create rewrite_dispatch_source_create_shim,
    13 dispatch_main rewrite_dispatch_main_shim,
    14 dispatch_read rewrite_dispatch_read_shim,
    15 dispatch_write rewrite_dispatch_write_shim,
    16 dispatch_io_create rewrite_dispatch_io_create_shim,
}
