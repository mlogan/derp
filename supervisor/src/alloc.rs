//! Deterministic allocator behind the interposed `malloc` family. Heap
//! layout is a function of the call sequence alone: one region at a fixed
//! address, bump allocation, intrusive per-class free lists. libmalloc's
//! per-CPU magazines would otherwise make addresses depend on the core a
//! thread happened to run on.
//!
//! Pointers outside the region (allocated before this dylib loaded, or by
//! this dylib itself) are forwarded to the real functions.

use std::ffi::c_void;

use crate::spin::SpinLock;

extern "C" {
    fn valloc(size: usize) -> *mut c_void;
    static mach_task_self_: u32;
    fn mach_vm_allocate(task: u32, addr: *mut u64, size: u64, flags: i32) -> i32;
}

/// Above the GPU carveout and below the scheduler's fixed region, where
/// nothing else lands; low addresses are taken now and then by whatever
/// the kernel maps first (see `shared::STUB_BASE`).
const REGION_HINT: usize = 0x74_0000_0000;
const REGION_SIZE: usize = 4 << 30;
const HEADER: usize = 16;
/// Small classes are multiples of 16 up to this size
const SMALL_MAX: usize = 1024;
const N_SMALL: usize = SMALL_MAX / 16;
/// Large classes are powers of two from 2 KB to 1 MB
const N_LARGE: usize = 10;
const PAGE: usize = 0x4000;

/// Header just below the payload: where the raw block starts and how big
/// the class is, so `free` can return the block regardless of alignment.
#[repr(C)]
struct Header {
    raw: usize,
    class_size: usize,
}

struct Heap {
    base: usize,
    bump: usize,
    end: usize,
    /// Intrusive lists: the first word of a free raw block links to the next
    small: [usize; N_SMALL],
    large: [usize; N_LARGE],
    /// Free page-multiple blocks: first word next, second word size
    huge: usize,
    broken: bool,
}

static HEAP: SpinLock<Heap> = SpinLock::new(Heap {
    base: 0,
    bump: 0,
    end: 0,
    small: [0; N_SMALL],
    large: [0; N_LARGE],
    huge: 0,
    broken: false,
});

/// In the child of a `fork`: see `SpinLock::force_unlock`.
pub fn forked() {
    HEAP.force_unlock();
}

/// True when the region is in use at its fixed address
pub fn region_fixed() -> bool {
    let h = HEAP.lock();
    h.base == REGION_HINT
}

impl Heap {
    fn init(&mut self) {
        if self.base != 0 || self.broken {
            return;
        }
        // A fixed Mach allocation fails instead of replacing what is there,
        // so MAP_FIXED over it is safe; an mmap hint alone is ignored by
        // the kernel now and then.
        let mut addr = REGION_HINT as u64;
        let reserved =
            unsafe { mach_vm_allocate(mach_task_self_, &raw mut addr, REGION_SIZE as u64, 0) } == 0;
        let fixed = if reserved { libc::MAP_FIXED } else { 0 };
        let p = unsafe {
            libc::mmap(
                REGION_HINT as *mut c_void,
                REGION_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON | fixed,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            self.broken = true;
            return;
        }
        if p as usize != REGION_HINT {
            crate::report::log(
                "heap region did not land at its fixed address; heap layout may vary",
            );
        }
        self.base = p as usize;
        self.bump = self.base;
        self.end = self.base + REGION_SIZE;
    }

    fn contains(&self, p: usize) -> bool {
        self.base != 0 && p >= self.base && p < self.end
    }

    /// Class index and size for a raw block of at least `raw_size` bytes
    fn class_for(raw_size: usize) -> (Option<usize>, usize) {
        if raw_size <= SMALL_MAX {
            let k = raw_size.div_ceil(16).max(1);
            return (Some(k - 1), k * 16);
        }
        let mut size = 2048;
        for i in 0..N_LARGE {
            if raw_size <= size {
                return (Some(N_SMALL + i), size);
            }
            size *= 2;
        }
        (None, raw_size.div_ceil(PAGE) * PAGE)
    }

    fn list(&mut self, class: usize) -> &mut usize {
        if class < N_SMALL {
            &mut self.small[class]
        } else {
            &mut self.large[class - N_SMALL]
        }
    }

    fn pop(&mut self, class: usize) -> Option<usize> {
        let head = *self.list(class);
        if head == 0 {
            return None;
        }
        let next = unsafe { *(head as *const usize) };
        *self.list(class) = next;
        Some(head)
    }

    fn push(&mut self, class: usize, raw: usize) {
        let head = *self.list(class);
        unsafe { *(raw as *mut usize) = head };
        *self.list(class) = raw;
    }

    fn pop_huge(&mut self, size: usize) -> Option<usize> {
        let mut prev: *mut usize = &raw mut self.huge;
        let mut cur = self.huge;
        while cur != 0 {
            let next = unsafe { *(cur as *const usize) };
            let cur_size = unsafe { *((cur + 8) as *const usize) };
            if cur_size == size {
                unsafe { *prev = next };
                return Some(cur);
            }
            prev = cur as *mut usize;
            cur = next;
        }
        None
    }

    fn alloc_raw(&mut self, raw_size: usize) -> Option<(usize, usize)> {
        let (class, size) = Self::class_for(raw_size);
        if let Some(c) = class {
            if let Some(p) = self.pop(c) {
                return Some((p, size));
            }
        } else if let Some(p) = self.pop_huge(size) {
            return Some((p, size));
        }
        if self.bump + size > self.end {
            return None;
        }
        let p = self.bump;
        self.bump += size;
        Some((p, size))
    }

    fn free_raw(&mut self, raw: usize, class_size: usize) {
        let (class, _) = Self::class_for(class_size);
        if let Some(c) = class {
            self.push(c, raw);
        } else {
            unsafe {
                *(raw as *mut usize) = self.huge;
                *((raw + 8) as *mut usize) = class_size;
            }
            self.huge = raw;
        }
    }

    /// Allocate `size` bytes at `align` (a power of two, at least 16)
    fn alloc(&mut self, size: usize, align: usize) -> *mut c_void {
        self.init();
        if self.base == 0 {
            return std::ptr::null_mut();
        }
        let slack = if align > 16 { align } else { 0 };
        let Some(wanted) = size.max(1).checked_add(HEADER + slack) else {
            return std::ptr::null_mut();
        };
        let Some((raw, class_size)) = self.alloc_raw(wanted) else {
            return std::ptr::null_mut();
        };
        let payload = (raw + HEADER + align - 1) & !(align - 1);
        debug_assert!(payload + size <= raw + class_size);
        unsafe {
            ((payload - HEADER) as *mut Header).write(Header { raw, class_size });
        }
        payload as *mut c_void
    }

    fn header(p: usize) -> Header {
        unsafe { ((p - HEADER) as *const Header).read() }
    }

    fn usable(p: usize) -> usize {
        let h = Self::header(p);
        h.raw + h.class_size - p
    }

    fn free(&mut self, p: usize) {
        let h = Self::header(p);
        self.free_raw(h.raw, h.class_size);
    }
}

fn in_region(p: *mut c_void) -> bool {
    HEAP.lock().contains(p as usize)
}

/// The deterministic heap is for the threads the scheduler runs. A GCD
/// worker allocates whenever real time has it running; sharing the heap
/// would make every address after that depend on the interleaving.
fn deterministic() -> bool {
    crate::sched::on_scheduled_thread()
}

pub extern "C" fn my_malloc(size: usize) -> *mut c_void {
    if !deterministic() {
        return unsafe { libc::malloc(size) };
    }
    HEAP.lock().alloc(size, 16)
}

pub extern "C" fn my_calloc(n: usize, size: usize) -> *mut c_void {
    if !deterministic() {
        return unsafe { libc::calloc(n, size) };
    }
    let Some(total) = n.checked_mul(size) else {
        return std::ptr::null_mut();
    };
    let p = HEAP.lock().alloc(total, 16);
    if !p.is_null() {
        unsafe { std::ptr::write_bytes(p.cast::<u8>(), 0, total) };
    }
    p
}

pub extern "C" fn my_free(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    let mut h = HEAP.lock();
    if h.contains(p as usize) {
        h.free(p as usize);
    } else {
        drop(h);
        unsafe { libc::free(p) };
    }
}

pub extern "C" fn my_realloc(p: *mut c_void, size: usize) -> *mut c_void {
    if p.is_null() {
        return my_malloc(size);
    }
    if !in_region(p) {
        return unsafe { libc::realloc(p, size) };
    }
    let old = Heap::usable(p as usize);
    if size <= old && size >= old / 2 {
        return p;
    }
    let q = my_malloc(size);
    if !q.is_null() {
        unsafe { std::ptr::copy_nonoverlapping(p.cast::<u8>(), q.cast::<u8>(), old.min(size)) };
        my_free(p);
    }
    q
}

pub extern "C" fn my_posix_memalign(
    out: *mut *mut c_void,
    align: usize,
    size: usize,
) -> libc::c_int {
    if !deterministic() {
        return unsafe { libc::posix_memalign(out, align, size) };
    }
    if !align.is_power_of_two() || align < std::mem::size_of::<usize>() {
        return libc::EINVAL;
    }
    let p = HEAP.lock().alloc(size, align.max(16));
    if p.is_null() {
        return libc::ENOMEM;
    }
    unsafe { *out = p };
    0
}

pub extern "C" fn my_aligned_alloc(align: usize, size: usize) -> *mut c_void {
    if !deterministic() {
        return unsafe { libc::aligned_alloc(align, size) };
    }
    if !align.is_power_of_two() {
        return std::ptr::null_mut();
    }
    HEAP.lock().alloc(size, align.max(16))
}

pub extern "C" fn my_valloc(size: usize) -> *mut c_void {
    if !deterministic() {
        return unsafe { valloc(size) };
    }
    HEAP.lock().alloc(size, PAGE)
}

pub extern "C" fn my_malloc_size(p: *const c_void) -> usize {
    if in_region(p.cast_mut()) {
        Heap::usable(p as usize)
    } else {
        unsafe { libc::malloc_size(p) }
    }
}

pub extern "C" fn my_malloc_good_size(size: usize) -> usize {
    let (_, class) = Heap::class_for(size.max(1) + HEADER);
    class - HEADER
}
