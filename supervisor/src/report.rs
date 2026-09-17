//! End-of-run report on the fd the launcher hands us, plus a stderr log.

use std::fmt::Write as _;

fn report_fd() -> Option<i32> {
    std::env::var("REWRITE_REPORT_FD").ok()?.parse().ok()
}

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

fn write_report() {
    let Some(fd) = report_fd() else { return };
    let mut text = String::new();
    crate::sched::report(&mut text);
    let _ = writeln!(text, "heap_fixed={}", crate::alloc::region_fixed());
    unsafe {
        libc::write(fd, text.as_ptr().cast(), text.len());
    }
}

extern "C" fn at_exit() {
    write_report();
}

pub fn install_exit_hook() {
    unsafe {
        libc::atexit(at_exit);
    }
}
