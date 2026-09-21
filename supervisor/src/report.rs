//! End-of-run report to the launcher, plus a stderr log.

use std::fmt::Write as _;

/// Write `msg` to stderr without allocating; interposers may log before
/// libmalloc is initialized.
pub fn log(msg: &str) {
    let mut buf = [0u8; 256];
    let prefix = b"supervisor: ";
    let n = msg.len().min(buf.len() - prefix.len() - 1);
    buf[..prefix.len()].copy_from_slice(prefix);
    buf[prefix.len()..prefix.len() + n].copy_from_slice(&msg.as_bytes()[..n]);
    buf[prefix.len() + n] = b'\n';
    unsafe {
        libc::write(2, buf.as_ptr().cast(), prefix.len() + n + 1);
    }
}

pub fn write_report() {
    if !crate::coord::connected() {
        return;
    }
    let mut text = String::new();
    crate::sched::report(&mut text);
    let _ = writeln!(text, "heap_fixed={}", crate::alloc::region_fixed());
    let leaked = |c: &std::sync::atomic::AtomicU64| c.load(std::sync::atomic::Ordering::Relaxed);
    let _ = writeln!(
        text,
        "heap_leaked_blocks={}",
        leaked(&crate::alloc::LEAKED_BLOCKS)
    );
    let _ = writeln!(
        text,
        "heap_leaked_bytes={}",
        leaked(&crate::alloc::LEAKED_BYTES)
    );
    crate::coord::report(&text);
}

extern "C" fn at_exit() {
    write_report();
}

pub fn install_exit_hook() {
    unsafe {
        libc::atexit(at_exit);
    }
}
