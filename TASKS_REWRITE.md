# Rewrite Experiment: Progress

Tracks `IMPLEMENTATION_PLAN_REWRITE.md`. Updated at the end of each work
unit.

## Layout (differs from the plan)

- the repository — crate `rewrite`: `macho.rs` (reader/writer), `launch.rs`
  (posix_spawn launcher), `rng.rs`, CLI in `main.rs`.
- `supervisor/` — crate `rewrite-supervisor`, the injected cdylib.
  It is a separate crate because the interposers and constructor must not
  be linked into the launcher binary.
- Guests must be linked with `-Wl,-headerpad,0x1000`: the rewriter adds two
  segment load commands (304 bytes) and default binaries have no room.

## Day 1 — Mach-O reader/writer and launcher ✅

- Parse header, load commands, segments, sections, `LC_FUNCTION_STARTS`,
  `LC_DATA_IN_CODE`.
- `emit()` inserts `__STUBD` (rw, one page) and `__STUB` (rx) in front of
  `__LINKEDIT`, shifts every linkedit file offset, drops the old
  `LC_CODE_SIGNATURE`; `adhoc_sign()` shells out to `codesign -s -`.
- Launcher spawns with `_POSIX_SPAWN_DISABLE_ASLR`, injects the dylib,
  reads a `key=value` report from an inherited pipe.
- Hello world round-trips and runs standalone and supervised
  (`tests/macho_tests.rs`).

## Day 2 — Branch rewriting, stubs, overhead ✅

- `decode.rs`: branch, memory, exclusive, stack-address classes with
  assembler-verified unit tests.
- `rewrite.rs`: one stub per hooked site; conditional sites keep an
  inverted guard inside the stub (reach never matters); `bl`/`blr` sites
  keep `bl` so x30 is set by hardware; memory sites replay the word.
- Supervisor trampoline saves x2-x17, nzcv, fpsr, q0-q31 around the Rust
  scheduler.
- Overhead on `loops 3`, branch-only, no supervisor: **1.64x** (target
  1.3x). Deferred: keep `b.cond` at the site when the stub is in reach,
  use x16/x17 at call sites instead of the stack.

## Day 3 — Threads, baton scheduler, blocking primitives ✅

- `sched.rs`: one mach semaphore per thread, baton handed at quantum expiry
  and blocking calls; RNG picks the next runnable thread and the quantum.
- `interpose.rs`: `pthread_create/join`, mutex (trylock loop), cond
  (FIFO waiter queues, timed waits released when idle), `__ulock_wait*`,
  `os_sync_wait_on_address*`, `dispatch_semaphore_wait` (Rust's
  `Thread::park`), `sched_yield`, sleeps.
- Thread exit: a pthread key destructor (created after dyld's TLV key, so
  it runs after Rust TLS destructors) hands the baton on. libpthread nulls
  the key value before the destructor, so the id is passed explicitly.
- Gotchas found: image 0 is the inserted dylib, not the executable (use
  `_NSGetMachExecuteHeader`); the real `pthread_join` waits on a ulock
  only the kernel wakes (pass-through mode); `os_unfair_lock` waiters
  yield-and-retry because the unlock only wakes if the kernel saw a waiter.
- `mutex.c`, `race.c` (both variants), `channel.rs` pass
  (`tests/threads_tests.rs`).

## Day 4 — Seeded quanta, trace, memory hooks ✅

- Sparse memory hooks with the stack and exclusive-span rules were built on
  day 2; schedule hash (FNV over from/to/issued) in the report.
- `race.c` with `--mem-hook-rate 1/16`: seed 30 (of this build) hooks the
  store and prints total=313294 with the same hash on every run; branch
  hooks only always print 400000. Test searches seeds 1..40 rather than
  hard-coding one.
- Known limitation: a lost update needs the *store* hooked; a hooked load
  replays after the switch, so it cannot split the read-modify-write.

## Day 5 — Determinism hardening, measurements, write-up ✅

- `supervisor/src/alloc.rs`: size-class allocator in a 4 GB region at
  0x3_0000_0000 behind the interposed malloc family; foreign pointers go
  to the real functions.
- `supervisor/src/determinism.rs`: seeded `arc4random*`, `getentropy`,
  `CCRandomGenerateBytes`; virtual clock behind `clock_gettime`,
  `gettimeofday`, `time`, `mach_absolute_time` (+1 µs per read, +1 ms per
  switch). mmap placement was tried and dropped (libmalloc xzone traps).
- `supervisor/src/spin.rs`: spinlock for state touched before libmalloc is
  initialized (`std::sync::Mutex` reads a thread-local, which dyld
  allocates with `malloc`).
- Schedule hash now covers the switch site (stub address or blocked-on
  address); `rewrite repeat --runs N` checks exit status, stdout and hash.
- 100-run identical checks: race.c (seed 30, total 313294), channel.rs,
  mutex.c (50), loops.c dense (20). All pass.
- Overhead on `loops 3`: branch-only 1.57x (1.62x supervised), 1/16 2.0x,
  rate 1 5.1x. Targets (1.3x, 3x) missed; see the write-up.
- Write-up: `docs/REWRITE_RESULTS.md`.

## Remaining (days 6-7 slack, not started)

- Stub cost: keep `b.cond` at the site when in reach; x16/x17 at call sites.
- Virtual-clock-driven timed waits.
- Blocking pass-through calls (`read` on a pipe) still hold the baton.
