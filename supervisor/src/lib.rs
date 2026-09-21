//! Supervisor dylib, injected into the guest with `DYLD_INSERT_LIBRARIES`.
//! Its constructor runs before the guest's `main`: it finds the rewritten
//! image's `__STUB` header, sets up the fixed region its stubs address,
//! joins the run's scheduler, and arranges for a report to be written at exit.

#![allow(clippy::missing_safety_doc)]

mod alloc;
mod coord;
mod determinism;
mod errno;
mod files;
mod gcd;
mod hostfs;
mod interpose;
mod io;
mod kq;
mod names;
mod net;
mod poll;
mod process;
mod report;
#[path = "../../src/rng.rs"]
mod rng;
mod sched;
mod signals;
// Also compiled into the launcher, which uses the parts that look dead here.
#[allow(dead_code)]
mod shared;
mod spin;
mod stubdata;

#[used]
#[link_section = "__DATA,__mod_init_func"]
static INIT: extern "C" fn() = init;

extern "C" fn init() {
    let config = sched::Config::from_env();
    let page = stubdata::find();
    if page.is_none() {
        report::log("main image is not rewritten; scheduling only at blocking calls");
    }
    determinism::init(config.seed);
    sched::init(page, &config);
    // Identical guests must not draw identical entropy. Process 0 keeps
    // the plain seed.
    determinism::init(sched::process_seed(config.seed, sched::pid()));
    report::install_exit_hook();
}
