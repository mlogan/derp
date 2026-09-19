//! The header the rewriter put at the start of the guest's `__STUB`
//! segment; its layout is defined by `rewrite::rewrite` (offsets duplicated
//! here on purpose so the dylib does not link the whole rewriter).

use std::ffi::{c_char, c_ulong};

pub const MAGIC: u64 = 0x0032_3030_5453_5752;
const OFF_MAGIC: usize = 0;
const OFF_SITES: usize = 8;
const OFF_MEM_SITES: usize = 16;
const OFF_SEED: usize = 24;
const HEADER_SIZE: c_ulong = 32;

extern "C" {
    fn _NSGetMachExecuteHeader() -> *const u8;
    fn getsegmentdata(mhp: *const u8, segname: *const c_char, size: *mut c_ulong) -> *mut u8;
}

#[derive(Clone, Copy)]
pub struct Info {
    pub sites: u64,
    pub mem_sites: u64,
    pub seed: u64,
}

/// Read the header from the main executable. Image index 0 is not usable:
/// with `DYLD_INSERT_LIBRARIES` the inserted dylib comes first.
pub fn find() -> Option<Info> {
    let mut size: c_ulong = 0;
    let base = unsafe {
        let mh = _NSGetMachExecuteHeader();
        getsegmentdata(mh, c"__STUB".as_ptr(), &raw mut size)
    };
    if base.is_null() || size < HEADER_SIZE {
        return None;
    }
    let at = |off: usize| unsafe { base.add(off).cast::<u64>().read_unaligned() };
    (at(OFF_MAGIC) == MAGIC).then(|| Info {
        sites: at(OFF_SITES),
        mem_sites: at(OFF_MEM_SITES),
        seed: at(OFF_SEED),
    })
}
