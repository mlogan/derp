//! Supervisor dylib, injected into the guest with `DYLD_INSERT_LIBRARIES`.

use std::io::Write;

#[used]
#[link_section = "__DATA,__mod_init_func"]
static INIT: extern "C" fn() = init;

extern "C" fn init() {
    if let Ok(fd) = std::env::var("REWRITE_REPORT_FD") {
        if let Ok(fd) = fd.parse::<i32>() {
            use std::os::unix::io::FromRawFd;
            let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
            let _ = writeln!(f, "supervisor=loaded");
            std::mem::forget(f);
        }
    }
}
