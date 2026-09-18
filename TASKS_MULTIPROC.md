# Multi-Process Deterministic Runs: Progress

Tracks `IMPLEMENTATION_PLAN_MULTIPROC.md`. Branch `mlogan-multiproc`
(branched from `mlogan-rewrite`, which is `origin/main` plus the plan).
Updated at the end of each work unit.

## Layout

- `supervisor/src/shared.rs` — the shared state and all scheduling
  decisions (pick, hand-off, condition queues, process death). Compiled
  into both crates: the launcher includes it with `#[path]`, as it already
  did for `rng.rs`.
- `supervisor/src/sched.rs` — per-process glue: mapping the state,
  thread ids, quantum installation, the register-saving trampoline.
- `src/coord.rs` — launcher side of the state: create, register,
  first baton, hand-off for dead processes, totals.
- `src/launch.rs` — `launch_run` (several guests) with `launch`
  (one guest) as a wrapper; `src/manifest.rs` — manifest parser.

## Day 1 — Shared scheduler, cross-process baton, manifest ✅

- State is a `#[repr(C)]` struct of indices only (1,024 threads, 256
  processes, about 64 KB) under a spinlock. Thread slots are never reused,
  so ids follow creation order.
- Parking is a shared compare-and-wait ulock on the thread's `park` word,
  called as `__ulock_wait2`/`__ulock_wake` directly.
- Condition variable FIFO queues became a `(cond_key, cond_seq)` pair on
  the thread record: signal picks the lowest sequence number. Same order,
  no separate fixed-capacity queue to size.
- The launcher pre-registers each initial process and its main thread,
  spawns them in manifest order, waits until all attached (checking the
  mapping address), then makes the first pick.
- **Process death is handled by the launcher, not the guest.** After the
  reap it retires the process's threads and, if one held the baton, picks
  the next holder (site `u64::MAX` in the trace). `exit`, `_exit`, `abort`
  and crashes are one path, and the guest's stdio flush at exit still
  happens under the baton. Children are watched with kqueue
  `EVFILT_PROC` on their pids, because `waitpid(-1)` would steal children
  from other runs in the same launcher process (parallel tests).
- Deadlock: the detecting guest aborts; the launcher finds nothing
  runnable after the reap, kills the survivors and reports it.
- Quantum counter is still per process (relocation is day 2): hand-off
  stores `pending_quantum` and whoever receives the baton installs it in
  its own `__STUBD` page. Hooks are accounted per process. This is also
  the fallback the plan names if the relocated counter costs too much.
- `rewrite run|repeat --manifest FILE [--scratch DIR]`. Program paths are
  relative to the manifest; `argv[0]` is the manifest token, not the
  rewritten file. Manifest runs get the scratch directory as cwd and
  `TMPDIR`; an existing scratch directory is cleared only if it carries
  our marker file. Single-program `run` keeps the caller's cwd.
- Report: `run.*` totals plus `p<i>.*` per process. `repeat` compares each
  guest's status and stdout and the run-wide hash.
- Standalone use of the dylib (no launcher) still works: it maps private
  state and hands itself the baton.
- Tests: `tests/multiproc_tests.rs`. Two `loops 1` processes: 6,817
  switches at seed 2, 100 identical runs, hashes differ across seeds,
  launcher CPU at 100% (one runnable thread at a time). A guest that
  aborts while holding the baton does not hang the other.
- `mutex.c`, `race.c`, `channel.rs` pass unchanged; the full suite ran 15
  times without a failure.

### Gotchas

- `os_sync_wait_on_address` is a libSystem wrapper: its `__ulock_wait2`
  lands in our own interposer. Raw ulock calls from the dylib are not
  rebound.
- `mmap` hints are not reliable. Low addresses vary because whatever the
  kernel places first above the dyld shared region differs between
  launches (about 1 launch in 100 under load), and the kernel ignores
  hints between roughly `0x5_0000_0000` and `0x70_0000_0000` (a hidden
  reservation, then the GPU carveout). The state is now reserved with a
  fixed `mach_vm_allocate` at `0x78_0000_0000` and mapped over with
  `MAP_FIXED`; 400/400 launches fixed.
- The supervisor allocator's region at `0x3_0000_0000` still uses a plain
  hint and only logs when it misses. Not seen missing in 400 launches of
  hello world; if it shows up, give it the same reservation treatment.

## Day 2 — Counter relocation, header room (in progress)

- [ ] Stub loads the counter through a pointer slot in `__STUBD`
- [ ] Counter in the shared state; standalone binaries point at a local word
- [ ] `__STUB` without a section header; slot in `__DATA` slack; drop
      `LC_UUID`/`LC_SOURCE_VERSION`/empty `LC_DATA_IN_CODE` when short
- [ ] Default-linked hello world rewrites and runs
- [ ] Overhead on `loops.c` recorded

## Days 3-12 — not started

See the plan's implementation order.
