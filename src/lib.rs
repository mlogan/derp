//! Native binary rewriting experiment: hook the branches and a sparse set
//! of memory accesses of an aarch64 Mach-O executable, then run it under a
//! supervisor that owns the thread schedule.

pub mod bisect;
pub mod cache;
pub mod coord;
pub mod decode;
pub mod hostdir;
pub mod launch;
pub mod macho;
pub mod manifest;
pub mod rewrite;
pub mod rng;
#[path = "../supervisor/src/shared.rs"]
pub mod shared;
pub mod stub;
