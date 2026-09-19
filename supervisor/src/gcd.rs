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

const NAMES: [&str; 17] = [
    "dispatch_async",
    "dispatch_async_f",
    "dispatch_after",
    "dispatch_after_f",
    "dispatch_apply",
    "dispatch_apply_f",
    "dispatch_group_async",
    "dispatch_group_async_f",
    "dispatch_barrier_async",
    "dispatch_barrier_async_f",
    "dispatch_group_notify",
    "dispatch_group_notify_f",
    "dispatch_source_create",
    "dispatch_main",
    "dispatch_read",
    "dispatch_write",
    "dispatch_io_create",
];

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

// The real functions, only ever reached by a branch from the shims.
extern "C" {
    pub fn dispatch_async();
    pub fn dispatch_async_f();
    pub fn dispatch_after();
    pub fn dispatch_after_f();
    pub fn dispatch_apply();
    pub fn dispatch_apply_f();
    pub fn dispatch_group_async();
    pub fn dispatch_group_async_f();
    pub fn dispatch_barrier_async();
    pub fn dispatch_barrier_async_f();
    pub fn dispatch_group_notify();
    pub fn dispatch_group_notify_f();
    pub fn dispatch_source_create();
    pub fn dispatch_main();
    pub fn dispatch_read();
    pub fn dispatch_write();
    pub fn dispatch_io_create();
}

extern "C" {
    pub fn rewrite_dispatch_async_shim();
    pub fn rewrite_dispatch_async_f_shim();
    pub fn rewrite_dispatch_after_shim();
    pub fn rewrite_dispatch_after_f_shim();
    pub fn rewrite_dispatch_apply_shim();
    pub fn rewrite_dispatch_apply_f_shim();
    pub fn rewrite_dispatch_group_async_shim();
    pub fn rewrite_dispatch_group_async_f_shim();
    pub fn rewrite_dispatch_barrier_async_shim();
    pub fn rewrite_dispatch_barrier_async_f_shim();
    pub fn rewrite_dispatch_group_notify_shim();
    pub fn rewrite_dispatch_group_notify_f_shim();
    pub fn rewrite_dispatch_source_create_shim();
    pub fn rewrite_dispatch_main_shim();
    pub fn rewrite_dispatch_read_shim();
    pub fn rewrite_dispatch_write_shim();
    pub fn rewrite_dispatch_io_create_shim();
}

// Each shim keeps the argument registers, asks `rewrite_gcd_check` about the
// caller, and continues into the real function as if nothing had happened.
std::arch::global_asm!(
    ".globl _rewrite_dispatch_async_shim",
    ".p2align 2",
    "_rewrite_dispatch_async_shim:",
    "stp x29, x30, [sp, #-16]!",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "mov x0, x30",
    "mov x1, #0",
    "bl _rewrite_gcd_check",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x0, x1, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "b _dispatch_async",
    ".globl _rewrite_dispatch_async_f_shim",
    ".p2align 2",
    "_rewrite_dispatch_async_f_shim:",
    "stp x29, x30, [sp, #-16]!",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "mov x0, x30",
    "mov x1, #1",
    "bl _rewrite_gcd_check",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x0, x1, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "b _dispatch_async_f",
    ".globl _rewrite_dispatch_after_shim",
    ".p2align 2",
    "_rewrite_dispatch_after_shim:",
    "stp x29, x30, [sp, #-16]!",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "mov x0, x30",
    "mov x1, #2",
    "bl _rewrite_gcd_check",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x0, x1, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "b _dispatch_after",
    ".globl _rewrite_dispatch_after_f_shim",
    ".p2align 2",
    "_rewrite_dispatch_after_f_shim:",
    "stp x29, x30, [sp, #-16]!",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "mov x0, x30",
    "mov x1, #3",
    "bl _rewrite_gcd_check",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x0, x1, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "b _dispatch_after_f",
    ".globl _rewrite_dispatch_apply_shim",
    ".p2align 2",
    "_rewrite_dispatch_apply_shim:",
    "stp x29, x30, [sp, #-16]!",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "mov x0, x30",
    "mov x1, #4",
    "bl _rewrite_gcd_check",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x0, x1, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "b _dispatch_apply",
    ".globl _rewrite_dispatch_apply_f_shim",
    ".p2align 2",
    "_rewrite_dispatch_apply_f_shim:",
    "stp x29, x30, [sp, #-16]!",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "mov x0, x30",
    "mov x1, #5",
    "bl _rewrite_gcd_check",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x0, x1, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "b _dispatch_apply_f",
    ".globl _rewrite_dispatch_group_async_shim",
    ".p2align 2",
    "_rewrite_dispatch_group_async_shim:",
    "stp x29, x30, [sp, #-16]!",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "mov x0, x30",
    "mov x1, #6",
    "bl _rewrite_gcd_check",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x0, x1, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "b _dispatch_group_async",
    ".globl _rewrite_dispatch_group_async_f_shim",
    ".p2align 2",
    "_rewrite_dispatch_group_async_f_shim:",
    "stp x29, x30, [sp, #-16]!",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "mov x0, x30",
    "mov x1, #7",
    "bl _rewrite_gcd_check",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x0, x1, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "b _dispatch_group_async_f",
    ".globl _rewrite_dispatch_barrier_async_shim",
    ".p2align 2",
    "_rewrite_dispatch_barrier_async_shim:",
    "stp x29, x30, [sp, #-16]!",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "mov x0, x30",
    "mov x1, #8",
    "bl _rewrite_gcd_check",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x0, x1, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "b _dispatch_barrier_async",
    ".globl _rewrite_dispatch_barrier_async_f_shim",
    ".p2align 2",
    "_rewrite_dispatch_barrier_async_f_shim:",
    "stp x29, x30, [sp, #-16]!",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "mov x0, x30",
    "mov x1, #9",
    "bl _rewrite_gcd_check",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x0, x1, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "b _dispatch_barrier_async_f",
    ".globl _rewrite_dispatch_group_notify_shim",
    ".p2align 2",
    "_rewrite_dispatch_group_notify_shim:",
    "stp x29, x30, [sp, #-16]!",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "mov x0, x30",
    "mov x1, #10",
    "bl _rewrite_gcd_check",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x0, x1, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "b _dispatch_group_notify",
    ".globl _rewrite_dispatch_group_notify_f_shim",
    ".p2align 2",
    "_rewrite_dispatch_group_notify_f_shim:",
    "stp x29, x30, [sp, #-16]!",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "mov x0, x30",
    "mov x1, #11",
    "bl _rewrite_gcd_check",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x0, x1, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "b _dispatch_group_notify_f",
    ".globl _rewrite_dispatch_source_create_shim",
    ".p2align 2",
    "_rewrite_dispatch_source_create_shim:",
    "stp x29, x30, [sp, #-16]!",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "mov x0, x30",
    "mov x1, #12",
    "bl _rewrite_gcd_check",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x0, x1, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "b _dispatch_source_create",
    ".globl _rewrite_dispatch_main_shim",
    ".p2align 2",
    "_rewrite_dispatch_main_shim:",
    "stp x29, x30, [sp, #-16]!",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "mov x0, x30",
    "mov x1, #13",
    "bl _rewrite_gcd_check",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x0, x1, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "b _dispatch_main",
    ".globl _rewrite_dispatch_read_shim",
    ".p2align 2",
    "_rewrite_dispatch_read_shim:",
    "stp x29, x30, [sp, #-16]!",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "mov x0, x30",
    "mov x1, #14",
    "bl _rewrite_gcd_check",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x0, x1, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "b _dispatch_read",
    ".globl _rewrite_dispatch_write_shim",
    ".p2align 2",
    "_rewrite_dispatch_write_shim:",
    "stp x29, x30, [sp, #-16]!",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "mov x0, x30",
    "mov x1, #15",
    "bl _rewrite_gcd_check",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x0, x1, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "b _dispatch_write",
    ".globl _rewrite_dispatch_io_create_shim",
    ".p2align 2",
    "_rewrite_dispatch_io_create_shim:",
    "stp x29, x30, [sp, #-16]!",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "mov x0, x30",
    "mov x1, #16",
    "bl _rewrite_gcd_check",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x0, x1, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "b _dispatch_io_create",
);
