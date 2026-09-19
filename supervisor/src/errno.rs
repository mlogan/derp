//! `errno`, for interposers that answer in libSystem's place.

use std::ffi::c_int;

pub fn get() -> c_int {
    unsafe { *libc::__error() }
}

/// Set `errno` and return the -1 that goes with it.
pub fn fail(e: c_int) -> c_int {
    unsafe { *libc::__error() = e };
    -1
}
