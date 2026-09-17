//! Supervisor dylib, injected into the guest with `DYLD_INSERT_LIBRARIES`.
//! Its constructor runs before the guest's `main`: it finds the rewritten
//! image's `__STUBD` page, fills the scheduler slot, seeds the quantum
//! counter, and arranges for a report to be written at exit.

#![allow(clippy::missing_safety_doc)]

mod report;
#[path = "../../src/rng.rs"]
mod rng;
mod sched;
mod stubdata;

#[used]
#[link_section = "__DATA,__mod_init_func"]
static INIT: extern "C" fn() = init;

extern "C" fn init() {
    let config = sched::Config::from_env();
    let Some(page) = stubdata::find() else {
        report::log("no __STUBD page in the main image; running unhooked");
        report::install_exit_hook();
        return;
    };
    sched::init(page, &config);
    report::install_exit_hook();
}
