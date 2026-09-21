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

## Second pass (2026-09-21)

Items from the reports that the first pass had not taken:

- Supervisor: the 1 ms real wait on a running owner's unfair lock gives
  up with a fatal report after 30 s; a fired one-shot outside
  registration leaves `Kq::external`; closing a descriptor purges kqueue
  entries whoever closes it; the mask is a slice set once and read
  without a lock.
- Allocator: a slab whose blocks are all free goes back to the window,
  unless its blocks are most of what its class has free (freeing and
  allocating one block in turn would otherwise take and release a slab
  every time). Free blocks off the pool are on a doubly linked list so
  that a slab's blocks can be taken out. The first-fit fallback jumps past
  taken slabs. The supervisor's own heap reuses the best-fitting large
  block and gives freed large blocks' pages back (`MADV_FREE`). Cost: the
  allocation benchmark went from 0.124 s to 0.142 s (native libmalloc
  0.193 s); `loops 3` is unchanged at 1.75x.
- bisect: the base rate is measured with a reseed before the first
  hand-off (it kept the original first choice of thread); the futures per
  probe grow with the base rate (x2 above 25%, x4 above 50%); both ends of
  the interval are measured again with other futures, and the output says
  when they disagree.
- Tools: `--native`, `--no-supervisor` and `--aslr` are refused by
  `bisect` and `suspects`; `--reseed` needs `--reseed-at`; a run started
  by a tool exits when the tool is gone, and its guests follow.
- `suspects` prints one `suspect=` line per site.
- Tests: the `suspects` tests require `atos`; a run file's `quantum:`
  reaches `bisect` (its reference fails at the plain run's nanosecond); a
  killed `bisect` leaves no guest behind. That test failed once, the
  first time it ran alongside the others, and passed 13 times since; the
  one race found (a run started as its tool died would never notice) is
  closed. Its failure message now lists what is left running.
- One constant per per-process stream, and `reseed_once` without one.
  A `timespec` helper. Three long comments trimmed.

Faults injected by the run file draw their first crash time when a
process is registered, before any reseed: a probe at time 0 keeps those
first crash times. Later ones are drawn from the reseeded stream.

## Fourth pass (2026-09-21): `execve` under the lock, and what it hid

Mark asked for a reproduction before a fix. What can hold the scheduler
lock without the baton is a signal handler on a parked thread: it runs at
a moment of real time, and a `write` to a pipe in it wakes the
scheduler's waiters, which takes the lock. So the guest is a worker that
takes signals and writes in the handler while main re-executes the
program 100 times (`tests/programs/exec_race.c`, `exec_race2.c`).

Two things came out:

1. **A handler on the thread that is inside the lock deadlocked on
   itself.** The worker is inside `yield_baton_as` holding the lock when
   the timer signal lands; the handler's `write` wakes waiters, which
   waits for the lock the same thread holds. 8 of 10 runs hung this way.
   The lock word now names the holder's thread as well as its pid
   (`owner_word`); `lock_unless_reentrant` tells a handler that it is on
   the holder, `with` then does nothing, and a wake the handler wanted is
   left in `Shared::deferred` for the holder to make as it leaves the lock
   (`Guard::drop`), with the state consistent again. A yield from such a
   handler is ignored, as one from a thread without the baton was.
2. **The `execve` case as predicted.** Sampled: the new image spinning in
   `join_run` on a lock word naming its own pid. `reclaim_after_exec`
   resets such a word before the new image's first lock: no thread of the
   new image can hold it yet. Validated on its own with `exec_race2`
   (main sends the worker a thread-directed signal whose handler writes
   2,000 times, and calls `execve` at once): 12 of 12 runs hung without
   the reclaim, 0 of 12 with it; the first guest gave 12 of 15 without
   the re-entrancy fix and 0 of 15 with it.

Found on the way, not ours: macOS kills a process that `execve`s with a
process-directed signal pending (or a real timer in flight) with SIGILL
before the new image's first instruction, natively too. The test guests
drain the timer or use thread-directed signals.

Observed once and not reproduced: during one full-suite run, right after
these changes, `a_reseeded_run_is_the_plain_run_until_the_reseed` found
two runs of one reseeded configuration with different traces. 30 further
runs of that test under CPU load and 5 further full suites were clean;
the failing run's traces were not kept. If it comes back, the two trace
files in its scratch directory are the evidence.

## Open

Nothing from the review.

## Third pass (2026-09-21): a guest's own children

Reproduced first, and worse than predicted: a parent that starts
`lost_update` and ends as it ended made `suspects` report "0 of 0 sites
are needed" and exit 0. The child's sites were outside the tool's
universe, so no mask ever reached them and every check passed.

Fixed: a process sends `MSG_IMAGE` with its executable's path once it
first holds the baton (a new image after `execve` does so too; a fork
child is recorded as running its parent's program). The launcher keeps
rewritten-to-original for every rewrite it made and reports
`p<i>.program` and `p<i>.image` for every process. `suspects` builds its
program list from the reference run's report, so a child's program, and
one only a child runs, is masked and symbolised like the entries'.
Test: `the_suspects_may_be_in_a_guests_own_child` names the child's racy
line.

Not done, by choice: reuse order of two freed blocks (A5), streams
restarting after `execve` (A8), and the larger refactors (a `Layout` enum
for `Heap`, splitting `my_kevent` and `main.rs`).
