//! Where a scheduled thread's mappings land is the run's to decide. The
//! kernel places an "anywhere" mapping first-fit in the address space as
//! it stands, and threads outside the schedule (GCD workers, whose stacks
//! the kernel makes) change what stands there at moments of real time. A
//! thread's stack address is its identity to code that hashes
//! `pthread_self` (RocksDB seeds its skip-list heights from it), so a run
//! whose thread stacks moved is another run.
//!
//! So a region of address space is reserved at start-up, and mapping
//! requests scheduled threads make without an address (libpthread's thread
//! stacks and `pthread_t` blocks through `mach_vm_map`, `mmap` for files
//! and anonymous memory) are placed in it in the order the schedule makes
//! them: first fit among what was given back, else the next unused stretch.
//! What is unmapped is reserved again, so the kernel never fills the hole
//! with something of its own. Threads outside the schedule keep the
//! kernel's placement. A region that is used up falls back to the kernel,
//! and the report says so.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::sched::on_scheduled_thread;
use crate::spin::SpinLock;

const BASE: u64 = 0x7C_0000_0000;
const SIZE: u64 = 64 << 30;
const PAGE: u64 = 0x4000;

const VM_FLAGS_FIXED: i32 = 0;
const VM_FLAGS_ANYWHERE: i32 = 1;
const VM_FLAGS_OVERWRITE: i32 = 0x4000;

extern "C" {
    static mach_task_self_: u32;
    pub fn mach_vm_allocate(task: u32, addr: *mut u64, size: u64, flags: i32) -> i32;
    pub fn mach_vm_deallocate(task: u32, addr: u64, size: u64) -> i32;
    pub fn mach_vm_map(
        task: u32,
        addr: *mut u64,
        size: u64,
        mask: u64,
        flags: i32,
        object: u32,
        offset: u64,
        copy: u32,
        cur_protection: i32,
        max_protection: i32,
        inheritance: u32,
    ) -> i32;
}

struct Region {
    /// First address never handed out
    next: u64,
    /// Stretches given back, `(address, length)`
    free: Vec<(u64, u64)>,
}

static REGION: SpinLock<Option<Region>> = SpinLock::new(None);
/// Mappings placed here, and requests the region could not take
pub static PLACED: AtomicU64 = AtomicU64::new(0);
pub static OVERFLOWED: AtomicU64 = AtomicU64::new(0);
static SAID_FULL: AtomicBool = AtomicBool::new(false);

/// Reserve the region. Without it (the range is taken in this process)
/// mappings stay the kernel's.
pub fn init() {
    let mut addr = BASE;
    if unsafe { mach_vm_allocate(mach_task_self_, &raw mut addr, SIZE, VM_FLAGS_FIXED) } != 0 {
        crate::report::log("the address range for scheduled threads' mappings is occupied; the kernel places them");
        return;
    }
    *REGION.lock() = Some(Region {
        next: BASE,
        free: Vec::new(),
    });
}

/// In the child of a `fork`: see `SpinLock::force_unlock`.
pub fn forked() {
    REGION.force_unlock();
}

fn round_up(v: u64) -> u64 {
    (v + PAGE - 1) & !(PAGE - 1)
}

pub fn in_region(addr: u64) -> bool {
    (BASE..BASE + SIZE).contains(&addr)
}

/// A stretch of `len` bytes (page-rounded) for the calling thread, if it
/// is scheduled and the region has room.
fn take(len: u64) -> Option<u64> {
    if !on_scheduled_thread() {
        return None;
    }
    let mut guard = REGION.lock();
    let region = guard.as_mut()?;
    if let Some(i) = region.free.iter().position(|&(_, l)| l >= len) {
        let (addr, l) = region.free[i];
        if l == len {
            region.free.remove(i);
        } else {
            region.free[i] = (addr + len, l - len);
        }
        return Some(addr);
    }
    if region.next + len > BASE + SIZE {
        OVERFLOWED.fetch_add(1, Ordering::Relaxed);
        if !SAID_FULL.swap(true, Ordering::Relaxed) {
            crate::report::log("the region for scheduled threads' mappings is full; the kernel places the rest");
        }
        return None;
    }
    let addr = region.next;
    region.next += len;
    Some(addr)
}

/// `[addr, addr + len)` is unmapped: reserve it again so the kernel never
/// fills it, and let the schedule reuse it (only if it was a scheduled
/// thread that let go: a thread outside the schedule does so at a moment
/// of real time, and its stretch is not reused).
fn give_back(addr: u64, len: u64) {
    let mut at = addr;
    unsafe { mach_vm_allocate(mach_task_self_, &raw mut at, len, VM_FLAGS_FIXED) };
    if !on_scheduled_thread() {
        return;
    }
    if let Some(region) = REGION.lock().as_mut() {
        region.free.push((addr, len));
    }
}

#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn my_mach_vm_map(
    task: u32,
    addr: *mut u64,
    size: u64,
    mask: u64,
    flags: i32,
    object: u32,
    offset: u64,
    copy: u32,
    cur_protection: i32,
    max_protection: i32,
    inheritance: u32,
) -> i32 {
    let real = |a: *mut u64, flags: i32| {
        mach_vm_map(
            task,
            a,
            size,
            mask,
            flags,
            object,
            offset,
            copy,
            cur_protection,
            max_protection,
            inheritance,
        )
    };
    if task == mach_task_self_ && flags & VM_FLAGS_ANYWHERE != 0 && mask < PAGE {
        let len = round_up(size);
        if let Some(at) = take(len) {
            let mut a = at;
            let placed = (flags & !VM_FLAGS_ANYWHERE) | VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE;
            let rc = real(&raw mut a, placed);
            if rc == 0 {
                PLACED.fetch_add(1, Ordering::Relaxed);
                *addr = a;
                return 0;
            }
            give_back(at, len);
        }
    }
    real(addr, flags)
}

pub unsafe extern "C" fn my_mach_vm_allocate(
    task: u32,
    addr: *mut u64,
    size: u64,
    flags: i32,
) -> i32 {
    if task == mach_task_self_ && flags & VM_FLAGS_ANYWHERE != 0 {
        let len = round_up(size);
        if let Some(at) = take(len) {
            let mut a = at;
            let placed = (flags & !VM_FLAGS_ANYWHERE) | VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE;
            if mach_vm_allocate(task, &raw mut a, size, placed) == 0 {
                PLACED.fetch_add(1, Ordering::Relaxed);
                *addr = a;
                return 0;
            }
            give_back(at, len);
        }
    }
    mach_vm_allocate(task, addr, size, flags)
}

pub unsafe extern "C" fn my_mach_vm_deallocate(task: u32, addr: u64, size: u64) -> i32 {
    let rc = mach_vm_deallocate(task, addr, size);
    if rc == 0 && task == mach_task_self_ && in_region(addr) {
        give_back(addr, round_up(size));
    }
    rc
}

pub unsafe extern "C" fn my_mmap(
    addr: *mut c_void,
    len: usize,
    prot: i32,
    flags: i32,
    fd: i32,
    offset: libc::off_t,
) -> *mut c_void {
    if addr.is_null() && flags & libc::MAP_FIXED == 0 {
        let rounded = round_up(len as u64);
        if let Some(at) = take(rounded) {
            let p = libc::mmap(at as *mut c_void, len, prot, flags | libc::MAP_FIXED, fd, offset);
            if p != libc::MAP_FAILED {
                PLACED.fetch_add(1, Ordering::Relaxed);
                return p;
            }
            give_back(at, rounded);
        }
    }
    libc::mmap(addr, len, prot, flags, fd, offset)
}

pub unsafe extern "C" fn my_munmap(addr: *mut c_void, len: usize) -> i32 {
    let rc = libc::munmap(addr, len);
    if rc == 0 && in_region(addr as u64) {
        give_back(addr as u64, round_up(len as u64));
    }
    rc
}
