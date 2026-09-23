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

const ENTROPY_STREAM: u64 = 0x5EED_5EED_5EED_5EED;

pub fn init(seed: u64) {
    *ENTROPY.lock() = Some(Rng::seed_from_u64(seed ^ ENTROPY_STREAM));
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
    // A child made after the run's reseed time switches at its first draw
    RESEEDED.store(false, std::sync::atomic::Ordering::Relaxed);
}

static RESEEDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The entropy stream, switched first if the run's reseed time has passed.
fn entropy() -> crate::spin::Guard<'static, Option<Rng>> {
    let reseed = crate::sched::reseed_once(&RESEEDED);
    let mut guard = ENTROPY.lock();
    if let Some(seed) = reseed {
        *guard = Some(Rng::seed_from_u64(seed ^ ENTROPY_STREAM));
    }
    guard
}

fn fill(buf: *mut u8, n: usize) {
    let mut guard = entropy();
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
    entropy()
        .get_or_insert_with(|| Rng::seed_from_u64(0))
        .below(u64::from(bound)) as u32
}

pub extern "C" fn my_arc4random_buf(buf: *mut c_void, n: usize) {
    crate::sched::diag_point(0xD1A6_0000_0000_0005);
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

/// The CPU's counter as the virtual clock would show it: a read like any
/// clock read (Redis takes its monotonic time from `cntvct_el0`).
pub fn counter_ticks() -> u64 {
    static FREQ: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let freq = *FREQ.get_or_init(|| {
        let f: u64;
        unsafe { std::arch::asm!("mrs {}, cntfrq_el0", out(reg) f) };
        f.max(1)
    });
    let ns = monotonic_ns();
    ns / 1_000_000_000 * freq + ns % 1_000_000_000 * freq / 1_000_000_000
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

pub extern "C" fn my_gettimeofday(tv: *mut libc::timeval, tz: *mut c_void) -> c_int {
    let ns = realtime_ns();
    if !tv.is_null() {
        unsafe {
            (*tv).tv_sec = (ns / 1_000_000_000) as libc::time_t;
            (*tv).tv_usec = ((ns % 1_000_000_000) / 1000) as libc::suseconds_t;
        }
    }
    // UTC, as the guest environment says (Redis reads its zone here)
    if !tz.is_null() {
        // struct timezone { int tz_minuteswest; int tz_dsttime; }
        unsafe { tz.cast::<[c_int; 2]>().write([0, 0]) };
    }
    0
}

// ---- interval timers ------------------------------------------------------

/// `setitimer(ITIMER_REAL)`: a virtual deadline of the process, at which
/// its SIGALRM becomes pending and a blocked thread of it is woken. The
/// other timers count CPU time and stay the kernel's.
pub unsafe extern "C" fn my_setitimer(
    which: c_int,
    new: *const libc::itimerval,
    old: *mut libc::itimerval,
) -> c_int {
    if which != libc::ITIMER_REAL || outside() || new.is_null() {
        return libc::setitimer(which, new, old);
    }
    let ns = |tv: libc::timeval| {
        (tv.tv_sec.max(0) as u64) * 1_000_000_000 + (tv.tv_usec.max(0) as u64) * 1000
    };
    let value = ns((*new).it_value);
    let interval = ns((*new).it_interval);
    let Some(now) = crate::sched::peek_clock() else {
        return libc::setitimer(which, new, old);
    };
    let before = crate::sched::with(|s, pid| {
        let p = &mut s.procs[pid as usize];
        let left = (p.alarm_at != 0).then(|| p.alarm_at.saturating_sub(now));
        let was = (left, p.alarm_interval);
        p.alarm_at = if value == 0 { 0 } else { now + value.max(1) };
        p.alarm_interval = if value == 0 { 0 } else { interval };
        was
    });
    if !old.is_null() {
        let (left, interval) = before.unwrap_or((None, 0));
        let tv = |ns: u64| libc::timeval {
            tv_sec: (ns / 1_000_000_000) as libc::time_t,
            tv_usec: ((ns % 1_000_000_000) / 1000) as libc::suseconds_t,
        };
        (*old).it_value = tv(left.unwrap_or(0));
        (*old).it_interval = tv(interval);
    }
    0
}

pub unsafe extern "C" fn my_getitimer(which: c_int, cur: *mut libc::itimerval) -> c_int {
    if which != libc::ITIMER_REAL || outside() || cur.is_null() {
        return libc::getitimer(which, cur);
    }
    let Some(now) = crate::sched::peek_clock() else {
        return libc::getitimer(which, cur);
    };
    let (left, interval) = crate::sched::with(|s, pid| {
        let p = &s.procs[pid as usize];
        (
            if p.alarm_at == 0 {
                0
            } else {
                p.alarm_at.saturating_sub(now)
            },
            p.alarm_interval,
        )
    })
    .unwrap_or((0, 0));
    let tv = |ns: u64| libc::timeval {
        tv_sec: (ns / 1_000_000_000) as libc::time_t,
        tv_usec: ((ns % 1_000_000_000) / 1000) as libc::suseconds_t,
    };
    (*cur).it_value = tv(left);
    (*cur).it_interval = tv(interval);
    0
}

pub unsafe extern "C" fn my_alarm(secs: libc::c_uint) -> libc::c_uint {
    let new = libc::itimerval {
        it_value: libc::timeval {
            tv_sec: secs as libc::time_t,
            tv_usec: 0,
        },
        it_interval: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
    };
    let mut old: libc::itimerval = std::mem::zeroed();
    my_setitimer(libc::ITIMER_REAL, &raw const new, &raw mut old);
    (old.it_value.tv_sec as libc::c_uint) + libc::c_uint::from(old.it_value.tv_usec > 0)
}

/// `getrusage`: CPU times are real time in disguise, so a scheduled thread
/// gets the virtual clock as user time and nothing else (Postgres reports
/// them in vacuum logs).
pub unsafe extern "C" fn my_getrusage(who: c_int, usage: *mut libc::rusage) -> c_int {
    if outside() || usage.is_null() {
        return libc::getrusage(who, usage);
    }
    let ns = now_ns();
    usage.write(std::mem::zeroed());
    (*usage).ru_utime.tv_sec = (ns / 1_000_000_000) as libc::time_t;
    (*usage).ru_utime.tv_usec = ((ns % 1_000_000_000) / 1000) as libc::suseconds_t;
    0
}

// ---- random devices -------------------------------------------------------

/// Descriptors open on `/dev/urandom` or `/dev/random`: reads of them
/// come from the entropy stream, like `getentropy`. Redis seeds its hash
/// tables from the device.
static RANDOM_FDS: SpinLock<Vec<c_int>> = SpinLock::new(Vec::new());

/// A path just opened as `fd`.
///
/// # Safety
/// `path` is a NUL-terminated string.
pub unsafe fn opened(path: *const libc::c_char, fd: c_int) {
    if fd < 0 || path.is_null() {
        return;
    }
    let name = std::ffi::CStr::from_ptr(path).to_bytes();
    if name == b"/dev/urandom" || name == b"/dev/random" {
        RANDOM_FDS.lock().push(fd);
    }
}

pub fn closed(fd: c_int) {
    RANDOM_FDS.lock().retain(|&f| f != fd);
}

pub fn duplicated(fd: c_int, new: c_int) {
    let mut fds = RANDOM_FDS.lock();
    if fds.contains(&fd) && !fds.contains(&new) {
        fds.push(new);
    }
}

/// `read(fd, buf, n)` for a random device, if `fd` is one and the reader
/// is scheduled: None otherwise.
pub fn read_random(fd: c_int, buf: *mut c_void, n: usize) -> Option<isize> {
    if outside() || !RANDOM_FDS.lock().contains(&fd) {
        return None;
    }
    fill(buf.cast(), n);
    Some(n as isize)
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
