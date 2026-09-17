//! Native binary rewriting experiment: hook the branches and a sparse set
//! of memory accesses of an aarch64 Mach-O executable, then run it under a
//! supervisor that owns the thread schedule.

pub mod decode;
pub mod launch;
pub mod macho;
pub mod rewrite;
pub mod rng;
pub mod stub;
