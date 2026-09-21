# A Heap Whose Layout Is a Function of the Seed

## Overview

The guest heap is deterministic, which it must be, but also predictable:
fresh blocks come from one bump pointer and freed blocks are reused last in
first out. Two allocations made one after the other always compare the
same way, on every seed. A bug that depends on how pointers compare (an
ordered map keyed by address, a lock order taken from addresses, `a < b`
assumed of two blocks) can then never be found, or never be seen to be
absent. Layout should be one more thing the seed decides, like the
schedule.

## Goals

1. Where a block lands is drawn from a stream seeded by the run's seed and
   the process's index: same seed, same addresses; another seed, another
   layout.
2. For two live blocks, of the same size class or not, `a < b` holds on
   some seeds and fails on others, with neither outcome rare.
3. Reuse is drawn too: `free(p)` followed by `malloc` of the same size does
   not always return `p`.
4. Nothing else changes: the region, the header, `malloc_size`, alignment,
   the leak rule for frees from outside the schedule, the supervisor's own
   heap (which stays compact).
5. Cost stays small; measure `loops` and an allocation-heavy guest.

## Design

- The 4 GB region is 65,536 slabs of 64 KB; a bitmap says which are taken.
- A size class up to 64 KB gets a slab at a random free index when it needs
  one, and carves it into slots. Every class keeps a pool of up to 64 (as
  built: 32, and slabs come from a 64 GB window of a 1 TB region)
  candidate blocks; `malloc` draws one at random, `free` returns a block to
  the pool (to an overflow list when the pool is full), and an empty pool
  is refilled from the overflow list, then from a new slab whose slots are
  taken in shuffled order.
- Larger blocks take a run of slabs starting at a random index and give
  the run back when freed.
- The stream is seeded once, before the first scheduled thread allocates.
  A `fork` child inherits the state; an `execve` reseeds from the same
  seed and process index.

## Non-goals

- Guard pages, quarantine, or detecting use after free.
- Randomising the stack or the image (ASLR stays off for guests).

## Tests

- `ptr_order.c`: pairs of blocks (same class, different classes, large),
  printing how they compare, and a reuse check. Over 16 seeds every
  comparison goes both ways and reuse is not always immediate; a seed
  gives the same answers twice.
- `ptr_order crash`: aborts if `a < b`. Some seeds abort, others pass, and
  a seed that aborts does so again.
- Unit tests of the heap: no overlap among live blocks, alignment,
  `usable`, runs returned and reused, same seed same addresses.
- The existing suites, and the heap-address repeatability test.

## Acceptance

All of the above, clippy clean, no measurable cost on `loops 3`, and the
allocation-heavy measurement recorded.
