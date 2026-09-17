//! Supervisor dylib, injected into the guest with `DYLD_INSERT_LIBRARIES`.
//! Its constructor runs before the guest's `main`: it finds the rewritten
//! image's `__STUBD` page, fills the scheduler slot, seeds the quantum
//! counter, and arranges for a report to be written at exit.

#![allow(clippy::missing_safety_doc)]

mod alloc;
mod determinism;
mod interpose;
mod report;
#[path = "../../src/rng.rs"]
mod rng;
mod sched;
mod spin;
mod stubdata;

#[used]
#[link_section = "__DATA,__mod_init_func"]
static INIT: extern "C" fn() = init;

extern "C" fn init() {
    let config = sched::Config::from_env();
    let page = stubdata::find();
    if page.is_none() {
        report::log("no __STUBD page in the main image; scheduling only at blocking calls");
    }
    determinism::init(config.seed);
    sched::init(page, &config);
    report::install_exit_hook();
}
