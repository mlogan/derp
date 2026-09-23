# Seed Bisection: Progress

Plan: `IMPLEMENTATION_PLAN_BISECT.md`. Branch `mlogan-seed-bisect`.

## Done (2026-09-20)

- **Reseed point** (`shared.rs`): `reseed_at`, `reseed_with`, `reseeded` in
  the shared state. At the first hand-off at or after `reseed_at` the
  scheduler's stream (thread choice, quanta) and the fault stream start
  over from `reseed_with`. `rewrite run --reseed-at T --reseed N`.
- **Report**: `p<i>.died_at` (virtual time the run learned of each death),
  `run.clock_ns`, and for a failed run `run.failure=entry <n>: <status>`
  with `run.failure_at`. The failing entry is chosen as the exit status
  already was: the first non-daemon entry whose last life did not exit 0.
- **`rewrite bisect --manifest FILE --seed S`** (`src/bisect.rs`):
  1. runs the seed once with a schedule trace; it must fail;
  2. base rate: `--runs` (20) futures reseeded at the very start;
  3. binary search over `[0, failure time]`: each probe is `--runs` futures
     reseeded at the midpoint, `--jobs` (4) at a time, each in its own
     scratch directory; a future counts when it ends with the reference's
     `run.failure`; "decided" is at least halfway between the base count
     and all of them;
  4. stops at `--resolution` (2 ms) and prints every probe, the interval,
     and the reference run's switches inside it, then
     `bisect.{failure_at_ns,base,probes,lo_ns,hi_ns}` for scripts.
  It refuses a seed that passes, and one that fails on 80% of futures from
  the start (no step to find). `--quantum`, `--mem-hook-rate` and
  `--net-latency` are passed to every run.
- **`tests/programs/latent.c`**: two workers meet at a gate, do three
  unsynchronised read-modify-writes each with 40 calls between read and
  write, print the window on their clock, work for 300 ms more; main
  aborts at the end if an update was lost. About 13% of seeds fail.

## Result on `latent.c` (seed 5)

```
reference: seed 5 fails (entry 0: signal 6) at 687.005 ms
base rate: 1 of 20 futures from the start fail the same way
probe   343.502 ms:  20 of 20 fail  decided by then
probe   171.751 ms:  20 of 20 fail  decided by then
probe    85.875 ms:  20 of 20 fail  decided by then
probe    42.937 ms:   2 of 20 fail  still open
probe    64.406 ms:   3 of 20 fail  still open
probe    75.141 ms:   3 of 20 fail  still open
probe    80.508 ms:   1 of 20 fail  still open
probe    83.192 ms:  20 of 20 fail  decided by then
probe    81.850 ms:   3 of 20 fail  still open
the failure is decided between 81.850 ms and 83.192 ms (3 of 20 fail before, 20 after)
switches of the failing run in that interval:
  p0 t2 -> t1 issued=448130 site=0x10000c6bc clock=83001000
```

The guest's own window was 82.0 to 87.0 ms and the abort came at 687 ms.
The one switch in the interval is the preemption between a worker's read
and its write. 200 runs, 1.2 s.

## Tests (`tests/bisect_tests.rs`)

- A reseeded run's switches before the reseed time equal the plain run's;
  after it they differ; two replacement seeds give two futures; the same
  one repeats, hash included.
- Bisection of the first failing seed of `latent.c`: the interval touches
  the window the guest printed (3 ms slack), lies before half the failure
  time, and is at most 2 ms wide. 15 of 15 repetitions passed.
- A passing seed is refused.

## Complete reseeding (2026-09-21, branch `mlogan-reseed-all`)

A reseed replaced only the scheduler's and the fault stream. A failure
decided by where heap blocks land, or by what `arc4random` returns, then
failed on every future, and bisect refused it as having no moment. Now
every stream starts over at the reseed time:

- The shared state already said whether the reseed time has passed and
  with what (`Shared::reseeded_with`, read without the lock: it is set at
  a hand-off, and those who ask are scheduled threads).
- Each per-process stream asks before it draws (`sched::reseed_once`) and
  switches once, to the replacement seed mixed with the process index and
  a constant of the stream: the guest heap's layout stream in
  `alloc::guest_heap`, the entropy stream in `determinism::entropy`. A
  process that starts after the reseed time switches at its first draw; a
  `fork` child's fresh entropy stream does too.
- Blocks that exist stay where they are: the past is fixed. The moment
  bisection finds for a layout bug is the allocation, not the use.
- There is no choice of streams. A probe is a complete reseed.

`late_draw.c` sleeps 60 ms, makes one draw that dooms it one time in
eight (three pairs of blocks all comparing `a < b`, or
`arc4random_uniform(8) == 0`), works 240 ms more and aborts. Both modes:
"decided between 59.766 ms and 60.938 ms", the draw being at 60.001 ms.
Test `bisection_finds_a_draw_from_a_process_stream`, 10 of 10 repetitions.

Still fixed by the seed and not reseedable: which memory instructions the
rewriter hooked (chosen before the run).

## Not done

- Run files only, no `rewrite run prog`.
- The signature is the failing entry and its status. Wrong output with a
  clean exit needs a user-supplied check command.
- A deadlocked future is not counted as the reference's failure, and a
  reference that deadlocks cannot be bisected (its report is not printed).
- A gradual step (several contributing events) is reported as one
  interval; read the probe table.
- The random streams restart after `execve` (review of PRs #4 to #10,
  finding A8): recorded, not fixed.
