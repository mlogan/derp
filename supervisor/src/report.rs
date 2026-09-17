//! End-of-run report on the fd the launcher hands us, plus a stderr log.

use std::io::Write;

fn report_fd() -> Option<i32> {
    std::env::var("REWRITE_REPORT_FD").ok()?.parse().ok()
}

pub fn log(msg: &str) {
    let _ = std::io::stderr().write_all(format!("supervisor: {msg}\n").as_bytes());
}

fn write_report() {
    let Some(fd) = report_fd() else { return };
    let mut text = String::new();
    crate::sched::report(&mut text);
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
