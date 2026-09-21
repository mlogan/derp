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

/// High up, where nothing else lands (low addresses are taken now and then
/// by whatever the kernel maps first; see `shared::STUB_BASE`). Address
/// space only: pages are touched as blocks are. Large, because a seeded
/// layout scatters what it hands out, and a small region would soon have
/// no room left for a big block between the scattered ones.
const REGION_HINT: usize = 0x2000_0000_0000;
const REGION_SIZE: usize = 1 << 40;
const HEADER: usize = 16;
/// Small classes are multiples of 16 up to this size
const SMALL_MAX: usize = 1024;
const N_SMALL: usize = SMALL_MAX / 16;
/// Large classes are powers of two from 2 KB to 1 MB
const N_LARGE: usize = 10;
const PAGE: usize = 0x4000;

/// A seeded heap hands out the region in slabs of this size
const SLAB: usize = 64 << 10;
/// Slabs for the pooled classes come from a window at one end of the
/// region (which end is the seed's choice) and runs from the rest: small
/// blocks scattered all over would leave no long runs free. It is also as
/// far as a pool's 32-bit offsets, in units of 16 bytes, reach.
const WINDOW: usize = 64 << 30;
/// Blocks a seeded `malloc` chooses among, per class
const POOL: usize = 32;
/// Classes whose blocks fit a slab: the small ones, and 2 KB to 64 KB
const N_POOLED: usize = N_SMALL + 6;

/// Header just below the payload: where the raw block starts and how big
/// the class is, so `free` can return the block regardless of alignment.
#[repr(C)]
struct Header {
    raw: usize,
    class_size: usize,
}

struct Heap {
    /// Where the region should go and how big it is
    hint: usize,
    size: usize,
    /// The guest's heap: its addresses are guest-visible, so a region that
    /// lands elsewhere is worth a log line, and where a block goes is drawn
    /// from the seed (see `draw`)
    guests: bool,
    /// Seeded before the first scheduled thread allocates; None keeps the
    /// compact layout (the supervisor's own heap, and unit tests of it)
    rng: Option<crate::rng::Rng>,
    /// Bitmap of taken slabs, mapped when the layout is seeded (an address)
    slabs: usize,
    /// First slab and length in slabs of the pooled classes' window
    window: (usize, usize),
    /// Scratch for shuffling a new slab's slots. Here and not on the stack:
    /// a guest may call `malloc` with a few kilobytes of stack left.
    order: [u16; SLAB / 16],
    /// Candidates `malloc` draws from, per class up to `SLAB`, as offsets
    /// into the region
    pools: [[u32; POOL]; N_POOLED],
    pool_len: [u8; N_POOLED],
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

impl Heap {
    const fn new(hint: usize, size: usize, guests: bool) -> Heap {
        Heap {
            hint,
            size,
            guests,
            rng: None,
            slabs: 0,
            window: (0, 0),
            order: [0; SLAB / 16],
            pools: [[0; POOL]; N_POOLED],
            pool_len: [0; N_POOLED],
            base: 0,
            bump: 0,
            end: 0,
            small: [0; N_SMALL],
            large: [0; N_LARGE],
            huge: 0,
            broken: false,
        }
    }
}

// The two heaps of a process live as long as it does; a heap made for a
// moment must give its terabyte of address space back.
impl Drop for Heap {
    fn drop(&mut self) {
        unsafe {
            if self.base != 0 {
                libc::munmap(self.base as *mut c_void, self.size);
            }
            if self.slabs != 0 {
                libc::munmap(self.slabs as *mut c_void, (self.size / SLAB).div_ceil(8));
            }
        }
    }
}

static HEAP: SpinLock<Heap> = SpinLock::new(Heap::new(REGION_HINT, REGION_SIZE, true));

/// The supervisor's own memory. Were it libmalloc's, our threads would
/// share libmalloc's locks with each other and with the system libraries,
/// in real time: a thread starting up, one that has handed the baton on,
/// and the baton holder collide there, the loser's wait reaches our ulock
/// interposer, and a collision that has nothing to do with the guest ends
/// up in its schedule. Behind our own spin lock nothing of the kind can be
/// seen from outside. It also makes allocating safe where libmalloc is not
/// (after `fork`, from inside libmalloc's own lock path).
static OWN: SpinLock<Heap> = SpinLock::new(Heap::new(OWN_HINT, OWN_SIZE, false));

/// Between the guest's heap and the scheduler's fixed region
const OWN_HINT: usize = 0x76_0000_0000;
const OWN_SIZE: usize = 1 << 30;

pub struct Private;

#[global_allocator]
static GLOBAL: Private = Private;

unsafe impl std::alloc::GlobalAlloc for Private {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let p = OWN.lock().alloc(layout.size(), layout.align().max(16));
        if p.is_null() {
            // No region to be had: libmalloc still works
            let mut out = std::ptr::null_mut();
            let align = layout.align().max(16);
            return if libc::posix_memalign(&raw mut out, align, layout.size()) == 0 {
                out.cast()
            } else {
                std::ptr::null_mut()
            };
        }
        p.cast()
    }

    unsafe fn dealloc(&self, p: *mut u8, _: std::alloc::Layout) {
        let mut own = OWN.lock();
        if own.contains(p as usize) {
            own.free(p as usize);
        } else {
            drop(own);
            libc::free(p.cast());
        }
    }
}

/// Whether `p` is the supervisor's own memory, which may reach the guest's
/// `free` when we hand a guest something we allocated.
fn in_own(p: *mut c_void) -> bool {
    OWN.lock().contains(p as usize)
}

static LAYOUT_RESEEDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The guest's heap, its layout stream switched first if the run's reseed
/// time has passed.
fn guest_heap() -> crate::spin::Guard<'static, Heap> {
    let reseed = crate::sched::reseed_once(&LAYOUT_RESEEDED, 0x4845_4150_4845_4150);
    let mut h = HEAP.lock();
    if let (Some(seed), true) = (reseed, h.rng.is_some()) {
        h.rng = Some(crate::rng::Rng::seed_from_u64(seed));
    }
    h
}

/// From now on, where a guest's block lands is drawn from `seed`. Call
/// before the first scheduled thread of the process allocates.
pub fn seed_layout(seed: u64) {
    let mut h = HEAP.lock();
    if h.base == 0 {
        h.rng = Some(crate::rng::Rng::seed_from_u64(seed));
    }
}

/// In the child of a `fork`: see `SpinLock::force_unlock`.
pub fn forked() {
    HEAP.force_unlock();
    OWN.force_unlock();
}

/// True when the region is in use at its fixed address
pub fn region_fixed() -> bool {
    let h = HEAP.lock();
    h.base == h.hint
}

impl Heap {
    fn init(&mut self) {
        if self.base != 0 || self.broken {
            return;
        }
        // A fixed Mach allocation fails instead of replacing what is there,
        // so MAP_FIXED over it is safe; an mmap hint alone is ignored by
        // the kernel now and then.
        let mut addr = self.hint as u64;
        let reserved =
            unsafe { mach_vm_allocate(mach_task_self_, &raw mut addr, self.size as u64, 0) } == 0;
        let fixed = if reserved { libc::MAP_FIXED } else { 0 };
        let p = unsafe {
            libc::mmap(
                self.hint as *mut c_void,
                self.size,
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
        // Logging allocates: never from inside the supervisor's own heap
        if p as usize != self.hint && self.guests {
            crate::report::log(
                "heap region did not land at its fixed address; heap layout may vary",
            );
        }
        self.base = p as usize;
        self.bump = self.base;
        self.end = self.base + self.size;
        if let Some(rng) = self.rng.as_mut() {
            let n_slabs = self.size / SLAB;
            let bitmap = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    n_slabs.div_ceil(8),
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANON,
                    -1,
                    0,
                )
            };
            if bitmap == libc::MAP_FAILED {
                self.broken = true;
                return;
            }
            self.slabs = bitmap as usize;
            let len = (WINDOW / SLAB).min(n_slabs / 4);
            let low = rng.below(2) == 0;
            self.window = (if low { 0 } else { n_slabs - len }, len);
        }
    }

    fn contains(&self, p: usize) -> bool {
        self.base != 0 && p >= self.base && p < self.end
    }

    /// Class index and size for a raw block of at least `raw_size` bytes
    /// None for a size no block can have
    fn class_for(raw_size: usize) -> Option<(Option<usize>, usize)> {
        if raw_size <= SMALL_MAX {
            let k = raw_size.div_ceil(16).max(1);
            return Some((Some(k - 1), k * 16));
        }
        let mut size = 2048;
        for i in 0..N_LARGE {
            if raw_size <= size {
                return Some((Some(N_SMALL + i), size));
            }
            size *= 2;
        }
        Some((None, raw_size.div_ceil(PAGE).checked_mul(PAGE)?))
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

    fn slab_word(&self, i: usize) -> *mut u64 {
        (self.slabs as *mut u64).wrapping_add(i / 64)
    }

    fn slab_taken(&self, i: usize) -> bool {
        unsafe { *self.slab_word(i) & (1 << (i % 64)) != 0 }
    }

    fn mark_slabs(&mut self, first: usize, n: usize, taken: bool) {
        for i in first..first + n {
            let word = self.slab_word(i);
            unsafe {
                if taken {
                    *word |= 1 << (i % 64);
                } else {
                    *word &= !(1 << (i % 64));
                }
            }
        }
    }

    /// A run of `n` free slabs at a random place, now taken: in the window
    /// for a pooled class's slab, outside it for a run.
    fn take_slabs(&mut self, n: usize, pooled: bool) -> Option<usize> {
        let (window_first, window_len) = self.window;
        let (first, len) = match (pooled, window_first) {
            (true, _) => self.window,
            (false, 0) => (window_len, self.size / SLAB - window_len),
            (false, _) => (0, window_first),
        };
        let last = len.checked_sub(n.max(1))?;
        if n == 0 {
            return None;
        }
        let free = |h: &Heap, at: usize| (at..at + n).all(|i| !h.slab_taken(i));
        let rng = self.rng.as_mut()?;
        // The region is mostly empty: a few draws find room. When they do
        // not, the first fit from one more drawn index does, if any exists.
        let mut tries = [0usize; 17];
        for t in &mut tries {
            *t = first + rng.below(last as u64 + 1) as usize;
        }
        let at = tries[..16]
            .iter()
            .copied()
            .find(|&at| free(self, at))
            .or_else(|| {
                (0..=last)
                    .map(|k| first + (tries[16] - first + k) % (last + 1))
                    .find(|&at| free(self, at))
            })?;
        self.mark_slabs(at, n, true);
        Some(self.base + at * SLAB)
    }

    /// A block of pooled class `c`, drawn from its candidates.
    fn draw(&mut self, c: usize, size: usize) -> Option<usize> {
        if self.pool_len[c] == 0 {
            self.refill(c, size)?;
        }
        let len = self.pool_len[c] as usize;
        // Half the time the newest candidate, which after a `free` is the
        // block just freed: programs meet immediate reuse and its absence
        let draw = self.rng.as_mut()?.below(2 * len as u64) as usize;
        let i = if draw < len { draw } else { len - 1 };
        let p = self.window_base() + self.pools[c][i] as usize * 16;
        self.pools[c][i] = self.pools[c][len - 1];
        self.pool_len[c] -= 1;
        Some(p)
    }

    /// Candidates for class `c`: freed blocks that overflowed the pool
    /// first, then a new slab at a random place, its slots in drawn order.
    fn refill(&mut self, c: usize, size: usize) -> Option<()> {
        while (self.pool_len[c] as usize) < POOL {
            let Some(p) = self.pop(c) else { break };
            self.offer(c, p);
        }
        if self.pool_len[c] > 0 {
            return Some(());
        }
        let slab = self.take_slabs(1, true)?;
        let n = SLAB / size;
        for i in 0..n {
            self.order[i] = i as u16;
        }
        for i in (1..n).rev() {
            let j = self.rng.as_mut()?.below(i as u64 + 1) as usize;
            self.order.swap(i, j);
        }
        for k in 0..n {
            let p = slab + self.order[k] as usize * size;
            if (self.pool_len[c] as usize) < POOL {
                self.offer(c, p);
            } else {
                self.push(c, p);
            }
        }
        Some(())
    }

    /// Add `p` to the candidates of class `c`, which has room.
    fn window_base(&self) -> usize {
        self.base + self.window.0 * SLAB
    }

    fn offer(&mut self, c: usize, p: usize) {
        self.pools[c][self.pool_len[c] as usize] = ((p - self.window_base()) / 16) as u32;
        self.pool_len[c] += 1;
    }

    fn alloc_raw(&mut self, raw_size: usize) -> Option<(usize, usize)> {
        let (class, size) = Self::class_for(raw_size)?;
        if self.rng.is_some() {
            return match class {
                Some(c) if c < N_POOLED => Some((self.draw(c, size)?, size)),
                _ => Some((self.take_slabs(size.div_ceil(SLAB), false)?, size)),
            };
        }
        if let Some(c) = class {
            if let Some(p) = self.pop(c) {
                return Some((p, size));
            }
        } else if let Some(p) = self.pop_huge(size) {
            return Some((p, size));
        }
        if self.bump.checked_add(size)? > self.end {
            return None;
        }
        let p = self.bump;
        self.bump += size;
        Some((p, size))
    }

    fn free_raw(&mut self, raw: usize, class_size: usize) {
        let Some((class, _)) = Self::class_for(class_size) else {
            return;
        };
        if self.rng.is_some() {
            match class {
                Some(c) if c < N_POOLED && (self.pool_len[c] as usize) < POOL => self.offer(c, raw),
                Some(c) if c < N_POOLED => self.push(c, raw),
                _ => self.mark_slabs((raw - self.base) / SLAB, class_size.div_ceil(SLAB), false),
            }
            return;
        }
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
    guest_heap().alloc(size, 16)
}

pub extern "C" fn my_calloc(n: usize, size: usize) -> *mut c_void {
    if !deterministic() {
        return unsafe { libc::calloc(n, size) };
    }
    let Some(total) = n.checked_mul(size) else {
        return std::ptr::null_mut();
    };
    let p = guest_heap().alloc(total, 16);
    if !p.is_null() {
        unsafe { std::ptr::write_bytes(p.cast::<u8>(), 0, total) };
    }
    p
}

/// Blocks given up because a thread outside the schedule freed them
pub static LEAKED_BLOCKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static LEAKED_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub extern "C" fn my_free(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    let mut h = HEAP.lock();
    if h.contains(p as usize) {
        // A thread outside the schedule (a GCD worker) frees whenever real
        // time has it running.
        // Putting the block back would reorder the free lists at that
        // moment, and with them every later address: it is leaked.
        if deterministic() {
            h.free(p as usize);
        } else {
            let size = Heap::usable(p as usize);
            LEAKED_BLOCKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            LEAKED_BYTES.fetch_add(size as u64, std::sync::atomic::Ordering::Relaxed);
        }
    } else {
        drop(h);
        let mut own = OWN.lock();
        if own.contains(p as usize) {
            // Something of ours that a guest was given to free
            own.free(p as usize);
        } else {
            drop(own);
            unsafe { libc::free(p) };
        }
    }
}

pub extern "C" fn my_realloc(p: *mut c_void, size: usize) -> *mut c_void {
    if p.is_null() {
        return my_malloc(size);
    }
    if !in_region(p) && !in_own(p) {
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
    let p = guest_heap().alloc(size, align.max(16));
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
    guest_heap().alloc(size, align.max(16))
}

pub extern "C" fn my_valloc(size: usize) -> *mut c_void {
    if !deterministic() {
        return unsafe { valloc(size) };
    }
    guest_heap().alloc(size, PAGE)
}

pub extern "C" fn my_malloc_size(p: *const c_void) -> usize {
    if in_region(p.cast_mut()) || in_own(p.cast_mut()) {
        Heap::usable(p as usize)
    } else {
        unsafe { libc::malloc_size(p) }
    }
}

pub extern "C" fn my_malloc_good_size(size: usize) -> usize {
    size.max(1)
        .checked_add(HEADER)
        .and_then(Heap::class_for)
        .map_or(size, |(_, class)| class - HEADER)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A seeded heap of its own, away from the process's two
    fn heap(seed: u64, hint: usize) -> Box<Heap> {
        let mut h = Box::new(Heap::new(hint, REGION_SIZE, false));
        h.rng = Some(crate::rng::Rng::seed_from_u64(seed));
        h
    }

    const SIZES: [usize; 9] = [1, 40, 64, 900, 1024, 5000, 60_000, 300_000, 3 << 20];

    fn addresses(seed: u64, hint: usize) -> Vec<usize> {
        let mut h = heap(seed, hint);
        let mut out = Vec::new();
        for round in 0..40 {
            let p = h.alloc(SIZES[round % SIZES.len()], 16) as usize;
            assert_ne!(p, 0);
            out.push(p - h.base);
            if round % 3 == 2 {
                h.free(p);
            }
        }
        out
    }

    #[test]
    fn a_seed_fixes_the_layout_and_seeds_differ() {
        assert_eq!(addresses(7, 0x60_0000_0000), addresses(7, 0x61_0000_0000));
        assert_ne!(addresses(7, 0x62_0000_0000), addresses(8, 0x63_0000_0000));
    }

    #[test]
    fn live_blocks_never_overlap_and_are_aligned() {
        let mut h = heap(3, 0x64_0000_0000);
        let mut live: Vec<(usize, usize)> = Vec::new();
        for round in 0..600usize {
            let size = SIZES[round % SIZES.len()] + round % 7;
            let align = if round % 5 == 0 { PAGE } else { 16 };
            let p = h.alloc(size, align) as usize;
            assert_ne!(p, 0);
            assert_eq!(p % align, 0);
            assert!(Heap::usable(p) >= size);
            assert!(h.contains(p) && h.contains(p + size - 1));
            for &(q, n) in &live {
                assert!(
                    p + size <= q || q + n <= p,
                    "{p:#x}+{size} overlaps {q:#x}+{n}"
                );
            }
            live.push((p, size));
            if round % 2 == 1 {
                let (q, _) = live.swap_remove(round * 31 % live.len());
                h.free(q);
            }
        }
    }

    #[test]
    fn two_blocks_compare_both_ways_across_seeds() {
        for sizes in [(40, 40), (24, 900), (5000, 300_000), (3 << 20, 3 << 20)] {
            let mut below = 0;
            for seed in 0..32 {
                let mut h = heap(seed, 0x65_0000_0000 + (seed as usize) * REGION_SIZE);
                let (a, b) = (h.alloc(sizes.0, 16) as usize, h.alloc(sizes.1, 16) as usize);
                below += usize::from(a < b);
            }
            assert!(
                (6..=26).contains(&below),
                "{sizes:?}: a<b on {below} of 32 seeds"
            );
        }
    }

    #[test]
    fn a_freed_block_comes_straight_back_only_sometimes() {
        let mut h = heap(5, 0x66_0000_0000 + 40 * REGION_SIZE);
        let mut back = 0;
        for _ in 0..200 {
            let p = h.alloc(64, 16) as usize;
            h.free(p);
            let q = h.alloc(64, 16) as usize;
            back += usize::from(p == q);
            h.free(q);
        }
        assert!((60..=140).contains(&back), "{back} of 200");
    }

    #[test]
    fn absurd_sizes_are_refused() {
        let mut h = heap(1, 0x68_0000_0000 + 90 * REGION_SIZE);
        for size in [
            usize::MAX,
            usize::MAX - 17,
            usize::MAX - 100,
            usize::MAX - 16_000,
            REGION_SIZE,
        ] {
            assert!(h.alloc(size, 16).is_null(), "{size:#x}");
        }
        assert!(!h.alloc(64, 16).is_null());
        assert_eq!(my_malloc_good_size(usize::MAX - 3), usize::MAX - 3);
    }

    /// Scattered small blocks must not use up the room for big ones.
    #[test]
    fn big_blocks_fit_after_many_small_ones() {
        for seed in 0..6 {
            let mut h = heap(seed, 0x69_0000_0000 + (100 + seed as usize) * REGION_SIZE);
            // 16,000 blocks of 40 KB: 625 MB live in about 16,000 slabs
            for _ in 0..16_000 {
                assert!(!h.alloc(40_000, 16).is_null());
            }
            for size in [8 << 20, 256 << 20, 4 << 30, 64usize << 30] {
                let p = h.alloc(size, 16);
                assert!(!p.is_null(), "seed {seed}: {size:#x}");
                h.free(p as usize);
            }
        }
    }

    #[test]
    fn a_freed_run_of_slabs_is_free_again() {
        let mut h = heap(9, 0x67_0000_0000 + 80 * REGION_SIZE);
        let taken = |h: &Heap| {
            (0..h.size / SLAB)
                .step_by(64)
                .map(|i| unsafe { (*h.slab_word(i)).count_ones() })
                .sum::<u32>()
        };
        let p = h.alloc(3 << 20, 16) as usize;
        assert_eq!(taken(&h), (3usize << 20).div_ceil(SLAB) as u32 + 1);
        h.free(p);
        assert_eq!(taken(&h), 0);
    }
}
