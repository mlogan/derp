# Native Binary Rewriting: Results

Outcome of the one-week experiment planned in `IMPLEMENTATION_PLAN_REWRITE.md`.
Code lives in the repository (rewriter, launcher, CLI) and `supervisor/`
(the injected dylib). Progress by day is in `TASKS_REWRITE.md`.

## The three questions

### 1. Is Mach-O rewriting reliable on real compiler output?

Yes for everything tried. `channel.rs` (Rust std, threads, mpsc, HashMap,
`println!`, built with `rustc -O`) rewrites and runs correctly:

| | channel.rs | loops.c | race.c |
|---|---|---|---|
| functions scanned (from `LC_FUNCTION_STARTS`) | 753 | 2 | 2 |
| text words | 65,734 | 358 | 55 |
| branch sites hooked | 2,502 | 16 | 1 |
| call sites hooked | 3,855 | 12 | 6 |
| memory candidates / hooked at 1/16 | 16,262 / 1,021 | 26 / 1 | 12 / 0 |
| words skipped (exclusive spans, data-in-code, `blr x30`) | 0 | 0 | 0 |
| stub bytes | 458 KB | 1.9 KB | 0.4 KB |
| file size, native → rewritten | 565 KB → 1,056 KB | | |

The skipped-word log is empty for all three, so the decoder classes cover
Clang and rustc output without any unknown instruction class turning up.
The rewritten binaries pass the kernel's ad-hoc signature check and run
standalone with the dylib absent (the scheduler slot is null, the counter
starts at zero and never expires).

Conditions the guest must meet: linked with `-Wl,-headerpad,0x1000` (two
segment load commands, 304 bytes, do not fit in a default header), no
hardened runtime or library validation (so `DYLD_INSERT_LIBRARIES` is
honoured).

### 2. What does hook overhead cost?

`loops.c` at scale 3 (sieve of 60M, 900×900 matmul, fib(33)), best of
three, release build of the tools, Apple Silicon:

| mode | no supervisor | supervised | plan target |
|---|---|---|---|
| native | 0.287 s | | |
| branch hooks only | 1.57x | 1.62x | 1.3x |
| memory hooks at 1/16 | 1.99x | 2.03x | |
| memory hooks at 1 | 5.1x | 5.07x | 3x |

The supervisor itself (quantum expiry every 1,000..10,000 hook events,
register save/restore, RNG) is within noise; the cost is the stubs. Both
targets are missed. The branch-only figure is dominated by `fib`, where
every call and return-side branch goes through a 15-instruction stub with
two stack stores and a counter load/store. Two optimisations were deferred
and would help: keep the condition at the site when the stub is within
`b.cond` reach (removes two branches on the not-taken path), and use the
x16/x17 scratch registers at call sites instead of the stack.

### 3. Does the scheduler reproduce a race from a seed?

Yes, and never with branch hooks only.

- `race.c` (two threads, 200,000 unsynchronised increments each): with
  branch hooks only, every seed tried prints 400000, because a switch can
  only land between iterations. With `--mem-hook-rate 1/16`, seeds that hook
  the store (seed 30 for this build; the test searches seeds 1..40 rather
  than hard-coding one) print `total=313294`, and 100 consecutive runs give
  the same total and schedule hash. The stack-shared variant behaves the
  same.
- A hooked *load* cannot split the read-modify-write: the stub replays the
  load after the switch, so the other thread's writes are seen. Only a
  hooked store exposes the lost update. This is inherent to replaying the
  displaced instruction and worth remembering when reading hit rates.
- `mutex.c` always prints 400000; `channel.rs` always prints the correct
  tallies; both have a stable schedule hash per seed and different hashes
  across seeds (`tests/threads_tests.rs`).

Determinism check (`rewrite repeat --runs N`, exit status + stdout +
schedule hash): race.c 100/100, channel.rs 100/100, mutex.c 50/50, loops.c
with dense memory hooks 20/20.

## What it took to get there

Things that were not in the plan and cost real time:

- **Image index 0 is the inserted dylib**, not the executable, when
  `DYLD_INSERT_LIBRARIES` is used. `_NSGetMachExecuteHeader` is the right
  way to find the main image.
- **libpthread nulls a key's value before calling its destructor**, so the
  thread-exit hook cannot look up its own id; it is passed as the value.
- **The real `pthread_join` waits on a ulock only the kernel wakes.**
  Interposing `__ulock_wait` turned that into a scheduler wait that nothing
  could satisfy. A per-thread pass-through mode wraps such calls.
- **`os_unfair_lock` unlock only calls `__ulock_wake` if the kernel set the
  waiter bit**, which never happens when waiters are parked in userland.
  Unfair-lock waits yield and retry instead of parking.
- **Rust's `Thread::park` is a `dispatch_semaphore`**, not a futex. A zero
  timeout is a try-wait, which lets the count stay in libdispatch while the
  blocking moves into the scheduler.
- **libmalloc's initializer calls `arc4random_uniform`** (via
  `mvm_guarded_range_init`) before the heap exists. Any code reachable from
  an entropy or malloc interposer must avoid `std::sync::Mutex`: its poison
  check reads a thread-local, and dyld allocates TLV storage with `malloc`.
  A spinlock and an allocation-free logger fixed it.
- **Hinting `mmap` addresses trips libmalloc's xzone range reservation**
  (`xzm_main_malloc_zone_init_range_groups` traps, about one run in six).
  mmap placement was dropped. With ASLR off and a deterministic call
  sequence the kernel's placement repeats anyway; the 100-run checks pass
  without it.

## Known limitations

- A blocking pass-through call (`read` on a pipe) blocks while holding the
  baton. Fine for these programs, wrong in general.
- Timed waits are released only when no thread is runnable; the virtual
  clock does not yet drive them.
- Thread-local destructors that run after our teardown hook (later
  destructor rounds) run unscheduled.
- `mach_absolute_time` assumes the Apple Silicon 125/3 timebase.
- Guest code that calls `malloc_zone_free` directly on a pointer from the
  deterministic heap would crash; nothing tried does.
- Only the program is rewritten. A quantum cannot expire inside libSystem;
  the interposed calls are the only scheduling points there.

## Verdict

The approach works on real binaries with a small amount of code (about
2,500 lines of Rust across both crates) and no dependencies beyond `libc`.
Rewriting is robust, scheduling is deterministic, and sparse memory hooks
do expose a lost update from a seed. The overhead is the weak point: 1.6x
for branch hooks alone against a 1.3x target, with clear paths to reduce
it. A Linux port (Syscall User Dispatch, static musl) would reuse the
decoder, stub emitter and scheduler unchanged and replace the Mach-O
writer and the interposition layer.
