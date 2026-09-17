//! Quantum accounting and (from day 3) the baton scheduler.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::rng::Rng;
use crate::stubdata::Page;

pub struct Config {
    pub seed: u64,
    pub quantum_lo: u32,
    pub quantum_hi: u32,
}

impl Config {
    pub fn from_env() -> Self {
        let seed = std::env::var("REWRITE_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let (mut lo, mut hi) = (1000, 10000);
        if let Ok(q) = std::env::var("REWRITE_QUANTUM") {
            if let Some((a, b)) = q.split_once("..") {
                if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
                    if a >= 1 && b >= a {
                        (lo, hi) = (a, b);
                    }
                }
            }
        }
        Config {
            seed,
            quantum_lo: lo,
            quantum_hi: hi,
        }
    }
}

struct State {
    page: Page,
    rng: Rng,
    quantum_lo: u32,
    quantum_hi: u32,
    /// Sum of every quantum handed out, for the hook count at exit
    issued: u64,
    switches: u64,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static SWITCHES: AtomicU64 = AtomicU64::new(0);

extern "C" {
    fn rewrite_scheduler_yield();
}

pub fn init(page: Page, cfg: &Config) {
    let mut rng = Rng::seed_from_u64(cfg.seed);
    let first = u64::from(rng.range_inclusive(cfg.quantum_lo, cfg.quantum_hi));
    unsafe {
        page.counter().write(first as i64);
        page.slot()
            .write(rewrite_scheduler_yield as *const () as usize);
    }
    *STATE.lock().unwrap() = Some(State {
        page,
        rng,
        quantum_lo: cfg.quantum_lo,
        quantum_hi: cfg.quantum_hi,
        issued: first,
        switches: 0,
    });
}

/// Called from the stub's expired path through the register-saving
/// trampoline, with the guest's registers already preserved.
#[no_mangle]
pub extern "C" fn rewrite_yield_impl() {
    let mut guard = STATE.lock().unwrap();
    let Some(st) = guard.as_mut() else { return };
    let q = u64::from(st.rng.range_inclusive(st.quantum_lo, st.quantum_hi));
    st.issued += q;
    st.switches += 1;
    SWITCHES.fetch_add(1, Ordering::Relaxed);
    unsafe { st.page.counter().write(q as i64) };
}

pub fn report(out: &mut String) {
    let guard = STATE.lock().unwrap();
    let Some(st) = guard.as_ref() else {
        let _ = writeln!(out, "supervisor=loaded");
        return;
    };
    let remaining = unsafe { st.page.counter().read() };
    let hooks = st.issued as i64 - remaining;
    let _ = writeln!(out, "supervisor=loaded");
    let _ = writeln!(out, "seed={}", st.page.seed());
    let _ = writeln!(out, "sites={}", st.page.sites());
    let _ = writeln!(out, "mem_sites={}", st.page.mem_sites());
    let _ = writeln!(out, "hooks={hooks}");
    let _ = writeln!(out, "switches={}", st.switches);
}

// Saves every register the guest may have live (the stub already saved x0,
// x1 and x30), calls the Rust scheduler, and restores them. x18 is the
// platform register and x19-x28 are callee-saved, so neither needs saving.
std::arch::global_asm!(
    ".globl _rewrite_scheduler_yield",
    ".p2align 2",
    "_rewrite_scheduler_yield:",
    "stp x29, x30, [sp, #-16]!",
    "mov x29, sp",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "stp x8, x9, [sp, #-16]!",
    "stp x10, x11, [sp, #-16]!",
    "stp x12, x13, [sp, #-16]!",
    "stp x14, x15, [sp, #-16]!",
    "stp x16, x17, [sp, #-16]!",
    "mrs x2, nzcv",
    "mrs x3, fpsr",
    "stp x2, x3, [sp, #-16]!",
    "stp q0, q1, [sp, #-32]!",
    "stp q2, q3, [sp, #-32]!",
    "stp q4, q5, [sp, #-32]!",
    "stp q6, q7, [sp, #-32]!",
    "stp q8, q9, [sp, #-32]!",
    "stp q10, q11, [sp, #-32]!",
    "stp q12, q13, [sp, #-32]!",
    "stp q14, q15, [sp, #-32]!",
    "stp q16, q17, [sp, #-32]!",
    "stp q18, q19, [sp, #-32]!",
    "stp q20, q21, [sp, #-32]!",
    "stp q22, q23, [sp, #-32]!",
    "stp q24, q25, [sp, #-32]!",
    "stp q26, q27, [sp, #-32]!",
    "stp q28, q29, [sp, #-32]!",
    "stp q30, q31, [sp, #-32]!",
    "bl _rewrite_yield_impl",
    "ldp q30, q31, [sp], #32",
    "ldp q28, q29, [sp], #32",
    "ldp q26, q27, [sp], #32",
    "ldp q24, q25, [sp], #32",
    "ldp q22, q23, [sp], #32",
    "ldp q20, q21, [sp], #32",
    "ldp q18, q19, [sp], #32",
    "ldp q16, q17, [sp], #32",
    "ldp q14, q15, [sp], #32",
    "ldp q12, q13, [sp], #32",
    "ldp q10, q11, [sp], #32",
    "ldp q8, q9, [sp], #32",
    "ldp q6, q7, [sp], #32",
    "ldp q4, q5, [sp], #32",
    "ldp q2, q3, [sp], #32",
    "ldp q0, q1, [sp], #32",
    "ldp x2, x3, [sp], #16",
    "msr nzcv, x2",
    "msr fpsr, x3",
    "ldp x16, x17, [sp], #16",
    "ldp x14, x15, [sp], #16",
    "ldp x12, x13, [sp], #16",
    "ldp x10, x11, [sp], #16",
    "ldp x8, x9, [sp], #16",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "ret",
);
