# Review of PRs #4 to #10: Findings and Fixes

Four reviewers (supervisor concurrency, allocator, the two debugging tools,
simplifications) on 2026-09-21. Each finding below was confirmed by the
reviewer's experiment unless marked (read). Status is filled in as fixes
land on branch `mlogan-review-fixes`.

## Tools (`bisect`, `suspects`)

- [ ] T1 `run.failure_at` is 0 when the failing entry is the last
  non-daemon of a run with a daemon; bisect then "finds" 0..0.
- [ ] T2 bisect ignores the run file's `seed:`, `quantum:`,
  `mem-hook-rate:`; suspects drops `--net-latency`.
- [ ] T3 suspects keys candidates per entry but masks per program name:
  two entries of one binary break it.
- [ ] T4 a program whose name contains `.rw` never matches its mask; the
  tool then reports "0 sites needed" and exits 0. Nothing checks that a
  mask took effect.
- [ ] T5 the reference run's environment differs from the probes'
  (`REWRITE_TRACE`, directory names of different length), which moves the
  guest's stack; and from the user's plain run (`REWRITE_MASK`).
- [ ] T6 a loop of only masked stubs never yields; no per-run timeout in
  either tool; a killed tool leaves children.
- [ ] T7 bisect prints counts it never measured and assumes the failure is
  decided at the end.
- [ ] T8 the signature is the lowest failing entry, so futures where the
  reference's entry fails the same way are miscounted.
- [ ] T9 `--runs 0` accepted; noisy at high base rates (documented).
- [ ] T10 sign-extending loads (`ldrsw` …) labelled "store".
- [ ] T13 (read) the sanity check compares signatures, not schedule hashes.

## Allocator

- [ ] A1 sizes near `usize::MAX` overflow in `class_for`: a live pointer to
  a zero-byte block.
- [ ] A2 random slab placement fragments the 4 GB region: a 32 MB `malloc`
  fails with 40 MB live.
- [ ] A3 8 KB stack array inlined into every allocation: guests with small
  thread stacks crash.
- [ ] A4 guest `pthread_key` destructors run after our teardown, outside
  the schedule; their frees are leaked (200 MB in the experiment).
- [ ] A6 (read) global-allocator fallback ignores alignment.
- A5 reuse is nearly last-in-first-out when two freed blocks both come
  back; A8 streams restart after `execve`: recorded, not fixed.

## Supervisor

- [ ] S1 a kqueue with a user event and an external registration waits in
  the kernel with the baton.
- [ ] S2 a `SIGCHLD` the thread has blocked at delivery is lost.
- [ ] S3 `recv(MSG_DONTWAIT)` on a blocking socket pair parks.
- [ ] S4 (= T6) masked loop livelock.
- [ ] S5 closing fd N drops timer/proc registrations whose ident is N.
- [ ] S6 `kill(getpid())` never reaches another thread when the caller
  blocks the signal.
- [ ] S7 the guest's `SA_SIGINFO` handler sees our `pthread_kill`'s
  siginfo, not the child's.
- [ ] S8 `SA_RESETHAND` reaches the kernel and disarms our delivery.
- [ ] S9 null event list with `EV_RECEIPT` panics the supervisor.
- [ ] S10 `EV_ENABLE` on `EV_CLEAR|EV_DISPATCH` does not re-fire.
- [ ] S11 receipts come back out of change order.
- [ ] S12 (read) the short unfair-lock wait drops the kernel's return.
- [ ] S13 a zombie that is not the waiter's child keeps the lock.
- [ ] S14 (read) kqueue registry entries leak on some closes.

## Simplifications and docs

- [ ] D1 misplaced doc comments (five), stale module docs, rule-breaking
  comments.
- [ ] D2 duplicated helpers: `sent()`, process seed mixing, pid probe,
  rwlock loops, timespec conversion, test helpers.
- [ ] D3 docs that disagree with the code (USAGE, CLAUDE.md, TASKS files).
- Larger refactors (shared runner for the tools, `my_kevent` split,
  `Heap` layout enum, `main.rs` split): deferred.
