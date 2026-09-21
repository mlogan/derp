# Review of PRs #4 to #10: Findings and Fixes

Four reviewers (supervisor concurrency, allocator, the two debugging tools,
simplifications) on 2026-09-21. Each finding below was confirmed by the
reviewer's experiment unless marked (read). Status is filled in as fixes
land on branch `mlogan-review-fixes`.

## Tools (`bisect`, `suspects`)

- [x] T1 `run.failure_at` is 0 when the failing entry is the last
  non-daemon of a run with a daemon; bisect then "finds" 0..0.
- [x] T2 bisect ignores the run file's `seed:`, `quantum:`,
  `mem-hook-rate:`; suspects drops `--net-latency`.
- [x] T3 suspects keys candidates per entry but masks per program name:
  two entries of one binary break it.
- [x] T4 a program whose name contains `.rw` never matches its mask; the
  tool then reports "0 sites needed" and exits 0. Nothing checks that a
  mask took effect.
- [x] T5 the reference run's environment differs from the probes'
  (`REWRITE_TRACE`, directory names of different length), which moves the
  guest's stack; and from the user's plain run (`REWRITE_MASK`).
- [x] T6 a loop of only masked stubs never yields; no per-run timeout in
  either tool; a killed tool leaves children.
- [x] T7 bisect prints counts it never measured and assumes the failure is
  decided at the end.
- [x] T8 the signature is the lowest failing entry, so futures where the
  reference's entry fails the same way are miscounted.
- [x] T9 `--runs 0` refused; the noise at high base rates is stated in
  `bisect.rs` and not otherwise addressed.
- [x] T10 sign-extending loads (`ldrsw` …) labelled "store".
- [x] T13 (read) the sanity check compares signatures, not schedule hashes.

## Allocator

- [x] A1 sizes near `usize::MAX` overflow in `class_for`: a live pointer to
  a zero-byte block.
- [x] A2 random slab placement fragments the 4 GB region: a 32 MB `malloc`
  fails with 40 MB live.
- [x] A3 8 KB stack array inlined into every allocation: guests with small
  thread stacks crash.
- [x] A4 guest `pthread_key` destructors run after our teardown, outside
  the schedule; their frees are leaked (200 MB in the experiment).
- [x] A6 (read) global-allocator fallback ignores alignment.
- A5 reuse is nearly last-in-first-out when two freed blocks both come
  back; A8 streams restart after `execve`: recorded, not fixed.

## Supervisor

- [x] S1 a kqueue with a user event and an external registration waits in
  the kernel with the baton.
- [x] S2 a `SIGCHLD` the thread has blocked at delivery is lost.
- [x] S3 `recv(MSG_DONTWAIT)` on a blocking socket pair parks.
- [x] S4 (= T6) masked loop livelock.
- [x] S5 closing fd N drops timer/proc registrations whose ident is N.
- [x] S6 `kill(getpid())` never reaches another thread when the caller
  blocks the signal.
- [x] S7 the guest's `SA_SIGINFO` handler sees our `pthread_kill`'s
  siginfo, not the child's.
- [x] S8 `SA_RESETHAND` reaches the kernel and disarms our delivery.
- [x] S9 null event list with `EV_RECEIPT` panics the supervisor.
- [x] S10 `EV_ENABLE` on `EV_CLEAR|EV_DISPATCH` does not re-fire.
- [x] S11 receipts come back out of change order.
- [x] S12 (read) the short unfair-lock wait drops the kernel's return.
- [x] S13 a zombie that is not the waiter's child keeps the lock.
- [x] S14 (read) kqueue registry entries leak on some closes.

## Simplifications and docs

- [x] D1 misplaced doc comments (five), stale module docs, rule-breaking
  comments.
- [x] D2 duplicated helpers: `sent()`, process seed mixing, pid probe,
  rwlock loops, timespec conversion, test helpers.
- [x] D3 docs that disagree with the code (USAGE, CLAUDE.md, TASKS files).
- Larger refactors (shared runner for the tools, `my_kevent` split,
  `Heap` layout enum, `main.rs` split): deferred.

## What was done (2026-09-21)

Every ticked item was re-run against the reviewer's own guest or scenario
after the fix; those guests are now `tests/programs/edge_*.c`,
`alloc_edges.c` and new cases in `bisect_tests.rs` and `suspects_tests.rs`.

- **One replay module** (`src/replay.rs`) under both tools: the command
  line over the run file's settings, spelled out for every run; a slot per
  job with paths of equal length; a timeout of 20 times the reference run;
  a failure counted when the reference's own entry ends the same way.
- **Trace and mask paths live in the shared state.** A guest's environment
  no longer differs between a plain run, a traced one and a masked one, so
  its stack does not move when the run is being looked at.
- **suspects' candidates are the sites where a quantum ended**, switch or
  not. The trace now records both. The first sanity check compares
  schedule hashes and caught this: masking a site where the quantum ended
  but the same thread went on still moves the run.
- **Heap**: 32 GB by default and configurable (`heap-size`), a window for
  pooled classes and the rest for runs, overflow checks, shuffle scratch
  off the stack.
- **Key destructors** run with the baton: `thread_teardown` re-arms its key
  for all but the last of libpthread's four rounds.
- **`process_lives`**: `proc_pidinfo` fails with `ESRCH` for a zombie
  (anyone's child) while `kill` still succeeds; the lock takeover and the
  orphan check both go by it.

Not done, by choice: reuse order of two freed blocks (A5), streams
restarting after `execve` (A8), the larger refactors, a fired one-shot
external registration staying in `Kq::external`, and `execve` with a
non-baton thread inside the scheduler lock.
