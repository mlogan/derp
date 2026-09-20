# Seeded Heap Layout: Progress

Plan: `IMPLEMENTATION_PLAN_HEAP.md`. Branch `mlogan-seeded-heap`.

## Done (2026-09-20)

- **`alloc.rs`**: the guest heap draws where blocks go from a stream seeded
  with the run's seed and the process index (`seed_layout`, called before
  the process's first scheduled thread exists). The supervisor's own heap
  keeps the compact layout.
  - The 4 GB region is 65,536 slabs of 64 KB with a bitmap.
  - Classes up to 64 KB: a pool of up to 32 candidates per class.
    `malloc` takes the newest candidate half the time (after a `free`,
    the block just freed) and a random one otherwise; `free` adds to the
    pool, or to an overflow list when it is full; an empty pool refills
    from the overflow list, then from a new slab at a random index whose
    slots are taken in shuffled order.
  - Larger blocks: a run of slabs at a random index (16 draws, then first
    fit from one more), given back on `free`.
  - Header, `malloc_size`, alignment and the leak rule are unchanged.
    A `fork` child carries the stream on; `execve` reseeds.
- **Tests**
  - `ptr_order.c` (`rewrite_tests`): four pairs of blocks (same class,
    small and large class, a slab class and a run, two 3 MB runs) each
    compare both ways over 16 seeds; a freed block comes straight back
    "sometimes"; every seed repeats; `ptr_order crash` (aborts if `a < b`)
    dies on some seeds, survives others, and a dying seed dies again.
  - Unit tests: same seed same addresses, seeds differ, live blocks never
    overlap and are aligned over 600 mixed operations, comparisons go both
    ways on 32 seeds for every pair of sizes, immediate reuse 30-70%,
    a freed run's slabs are free.
  - Full suites twice; the heap-address repeatability test at 1,000 and
    600 runs per seed under load: one outcome per seed.

## Measurements (release, best of 5)

| | time | |
|---|---|---|
| `loops 3` native / rewritten | 0.269 s / 0.469 s | 1.74x, unchanged |
| 4M malloc+free, 20,000 live, native libmalloc | 0.191 s | |
| same, supervised, compact layout (before) | 0.107 s | |
| same, supervised, seeded layout | 0.119 s | +11% on pure allocation |

## Notes

- A C compiler may assume a freed pointer equals nothing: `p == q` after
  `free(p)` folded to false at `-O1`. `ptr_order.c` compares integers
  taken before the free.
- Shuffled slots touch a slab's pages in random order, so a class's first
  slab costs up to 64 KB of resident memory sooner than a bump would.

## Not done

- No switch to turn it off. If a guest ever needs the compact layout, a
  run-file key is a few lines (`seed_layout` is simply not called).
- Stack and image addresses stay fixed (ASLR off); only the heap varies.
