//! The `__STUBD` page the rewriter placed in the guest image; its layout is
//! defined by `rewrite::rewrite` (offsets duplicated here on purpose so the
//! dylib does not link the whole rewriter).

use std::ffi::{c_char, c_ulong};

pub const MAGIC: u64 = 0x0031_3030_5453_5752;
const OFF_COUNTER: usize = 0;
const OFF_SLOT: usize = 8;
const OFF_MAGIC: usize = 16;
const OFF_SITES: usize = 24;
const OFF_MEM_SITES: usize = 32;
const OFF_SEED: usize = 40;

extern "C" {
    fn _NSGetMachExecuteHeader() -> *const u8;
    fn getsegmentdata(mhp: *const u8, segname: *const c_char, size: *mut c_ulong) -> *mut u8;
}

#[derive(Clone, Copy)]
pub struct Page {
    base: *mut u8,
}

unsafe impl Send for Page {}
unsafe impl Sync for Page {}

impl Page {
    pub fn base(self) -> usize {
        self.base as usize
    }

    pub fn from_base(base: usize) -> Option<Page> {
        (base != 0).then_some(Page {
            base: base as *mut u8,
        })
    }

    pub fn counter(self) -> *mut i64 {
        unsafe { self.base.add(OFF_COUNTER).cast() }
    }

    pub fn slot(self) -> *mut usize {
        unsafe { self.base.add(OFF_SLOT).cast() }
    }

    fn u64_at(self, off: usize) -> u64 {
        unsafe { self.base.add(off).cast::<u64>().read_unaligned() }
    }

    pub fn sites(self) -> u64 {
        self.u64_at(OFF_SITES)
    }

    pub fn mem_sites(self) -> u64 {
        self.u64_at(OFF_MEM_SITES)
    }

    pub fn seed(self) -> u64 {
        self.u64_at(OFF_SEED)
    }
}

/// Locate the page in the main executable. Image index 0 is not usable:
/// with `DYLD_INSERT_LIBRARIES` the inserted dylib comes first.
pub fn find() -> Option<Page> {
    let mut size: c_ulong = 0;
    let base = unsafe {
        let mh = _NSGetMachExecuteHeader();
        getsegmentdata(mh, c"__STUBD".as_ptr(), &raw mut size)
    };
    if base.is_null() || size < 48 {
        return None;
    }
    let page = Page { base };
    (page.u64_at(OFF_MAGIC) == MAGIC).then_some(page)
}
