//! The remaining sources of run-to-run variation that the guest can
//! observe: entropy and the clock.

use crate::spin::SpinLock;
use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::rng::Rng;

// mmap placement was tried and dropped: libmalloc's lazily initialized
// xzone allocator reserves address ranges of its own choosing and traps
// (xzm_main_malloc_zone_init_range_groups) when hinted mappings are in its
// way. With ASLR off and a deterministic call sequence the kernel's own
// placement repeats anyway.

// ---- entropy --------------------------------------------------------------

static ENTROPY: SpinLock<Option<Rng>> = SpinLock::new(None);

pub fn init(seed: u64) {
    *ENTROPY.lock() = Some(Rng::seed_from_u64(seed ^ 0x5EED_5EED_5EED_5EED));
}

/// The seeded stream is for scheduled threads. A GCD worker draws at a
/// moment real time picks, which would shift every later draw.
fn outside() -> bool {
    !crate::sched::on_scheduled_thread()
}

/// In the child of a `fork`: its own stream (the real `arc4random` reseeds
/// on fork too), and see `SpinLock::force_unlock`.
pub fn forked(seed: u64) {
    ENTROPY.force_unlock();
    init(seed);
}

fn fill(buf: *mut u8, n: usize) {
    let mut guard = ENTROPY.lock();
    let rng = guard.get_or_insert_with(|| Rng::seed_from_u64(0));
    let mut i = 0;
    while i < n {
        let word = rng.next_u64().to_le_bytes();
        let take = (n - i).min(8);
        unsafe { std::ptr::copy_nonoverlapping(word.as_ptr(), buf.add(i), take) };
        i += take;
    }
}

// Outside the scheduler each call goes to its own real counterpart, never
// to another entropy function: corecrypto's initialization calls
// `getentropy`, and answering that with `arc4random_buf` re-enters
// corecrypto inside its once-gate, which aborts the process at startup.

pub extern "C" fn my_arc4random() -> u32 {
    if outside() {
        return unsafe { libc::arc4random() };
    }
    let mut v = [0u8; 4];
    fill(v.as_mut_ptr(), 4);
    u32::from_le_bytes(v)
}

pub extern "C" fn my_arc4random_uniform(bound: u32) -> u32 {
    if bound < 2 {
        return 0;
    }
    if outside() {
        return unsafe { libc::arc4random_uniform(bound) };
    }
    ENTROPY
        .lock()
        .get_or_insert_with(|| Rng::seed_from_u64(0))
        .below(u64::from(bound)) as u32
}

pub extern "C" fn my_arc4random_buf(buf: *mut c_void, n: usize) {
    if outside() {
        return unsafe { libc::arc4random_buf(buf, n) };
    }
    fill(buf.cast(), n);
}

pub extern "C" fn my_getentropy(buf: *mut c_void, n: usize) -> c_int {
    if outside() {
        return unsafe { libc::getentropy(buf, n) };
    }
    if n > 256 {
        unsafe { *libc::__error() = libc::EIO };
        return -1;
    }
    fill(buf.cast(), n);
    0
}

extern "C" {
    fn CCRandomGenerateBytes(buf: *mut c_void, n: usize) -> c_int;
}

pub extern "C" fn my_cc_random_generate_bytes(buf: *mut c_void, n: usize) -> c_int {
    if outside() {
        return unsafe { CCRandomGenerateBytes(buf, n) };
    }
    fill(buf.cast(), n);
    0
}

// ---- virtual clock --------------------------------------------------------

/// The clock is the run's, in the shared scheduler state, so every process
/// sees one timeline. This local one only serves a process that is not in
/// a run (passive mode).
static LOCAL_TICKS: AtomicU64 = AtomicU64::new(0);
/// Fixed wall-clock epoch offset so `CLOCK_REALTIME` is repeatable too
pub const REALTIME_BASE_NS: u64 = 1_800_000_000 * 1_000_000_000;
pub const MONOTONIC_BASE_NS: u64 = 1_000_000_000;

fn now_ns() -> u64 {
    // A thread the scheduler does not run (a GCD worker) sees the time but
    // does not move it: when, and whether, it reads the clock depends on
    // real time, and every read is a tick for everyone.
    if !crate::sched::on_scheduled_thread() {
        if let Some(now) = crate::sched::peek_clock() {
            return now;
        }
    }
    crate::sched::clock_read().unwrap_or_else(|| {
        LOCAL_TICKS.fetch_add(crate::shared::PER_READ_NS, Ordering::Relaxed)
            + crate::shared::PER_READ_NS
    })
}

fn monotonic_ns() -> u64 {
    MONOTONIC_BASE_NS + now_ns()
}

fn realtime_ns() -> u64 {
    REALTIME_BASE_NS + now_ns()
}

fn is_realtime(clk: libc::clockid_t) -> bool {
    clk == libc::CLOCK_REALTIME
}

pub extern "C" fn my_clock_gettime(clk: libc::clockid_t, ts: *mut libc::timespec) -> c_int {
    let ns = if is_realtime(clk) {
        realtime_ns()
    } else {
        monotonic_ns()
    };
    unsafe {
        (*ts).tv_sec = (ns / 1_000_000_000) as libc::time_t;
        (*ts).tv_nsec = (ns % 1_000_000_000) as libc::c_long;
    }
    0
}

pub extern "C" fn my_clock_gettime_nsec_np(clk: libc::clockid_t) -> u64 {
    if is_realtime(clk) {
        realtime_ns()
    } else {
        monotonic_ns()
    }
}

pub extern "C" fn my_gettimeofday(tv: *mut libc::timeval, _tz: *mut c_void) -> c_int {
    let ns = realtime_ns();
    if !tv.is_null() {
        unsafe {
            (*tv).tv_sec = (ns / 1_000_000_000) as libc::time_t;
            (*tv).tv_usec = ((ns % 1_000_000_000) / 1000) as libc::suseconds_t;
        }
    }
    0
}

pub extern "C" fn my_time(t: *mut libc::time_t) -> libc::time_t {
    let s = (realtime_ns() / 1_000_000_000) as libc::time_t;
    if !t.is_null() {
        unsafe { *t = s };
    }
    s
}

/// Apple Silicon's timebase is 125/3 (24 MHz), so absolute time in ticks is
/// nanoseconds scaled by 3/125.
pub extern "C" fn my_mach_absolute_time() -> u64 {
    monotonic_ns() * 3 / 125
}

pub extern "C" fn my_mach_continuous_time() -> u64 {
    monotonic_ns() * 3 / 125
}
