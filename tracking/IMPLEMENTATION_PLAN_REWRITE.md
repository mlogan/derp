# Native Binary Rewriting: One-Week Experiment

## Overview

Instead of compiling programs to DVM bytecode, run real ARM64 executables
under a supervisor that owns their system calls and their thread schedule.
The executable's text is rewritten into a new on-disk Mach-O: every branch
becomes a branch to a stub that counts down a quantum and enters the
scheduler when it expires; a pseudo-random subset of loads and stores get
the same treatment so switches can land inside basic blocks. A supervisor
dylib is injected at launch; it owns the schedule and intercepts the C
library. Threads are real OS threads that run one at a time, handing a
baton between them at those switch points.

The experiment answers three questions with numbers:

1. How reliable is Mach-O rewriting on real compiler output (a static-ish
   Rust binary with std, linked against libSystem)?
2. What does the hook overhead cost, with branch hooks only and with sparse
   memory hooks?
3. Does the scheduler reproduce a two-thread lost-update race from a seed,
   and never reproduce it with branch hooks only?

## Decisions

- **Target: aarch64 macOS (Apple Silicon), Mach-O executables.** Built with
  the local toolchain: C via `clang`, Rust via `aarch64-apple-darwin`.
  macOS has no static libc, so every guest links libSystem dynamically;
  that boundary is exactly where we intercept, so it is a feature, not an
  obstacle. Development is native on the Mac, no container. Linux (Syscall
  User Dispatch, in-memory patching, static musl) is a possible follow-up
  that would reuse the rewriter and scheduler unchanged.
- **Interception at the libSystem boundary via dyld interposing.** macOS
  programs never issue raw `svc`; they call `write`, `read`,
  `pthread_create`, `pthread_mutex_lock` in libSystem. A supervisor dylib
  injected through `DYLD_INSERT_LIBRARIES` uses dyld interposing to replace
  those functions. Interposing also catches libSystem's *internal* calls,
  so the stdout lock inside `printf` is visible to the scheduler.
- **Rewriting produces a new signed Mach-O on disk.** Apple Silicon will
  not execute a modified page of a signed binary, and a process cannot turn
  its own signed text into a `MAP_JIT` region. So the rewriter reads the
  guest, patches the branches, appends a stub segment within `b` range of
  `__text`, and ad-hoc signs the result. Rewriting is a build step, not a
  load-time step. Only the program is rewritten; its dylibs are not.
- **Real threads, one at a time, baton-scheduled.** Each guest thread is a
  real pthread parked on a semaphore; the scheduler hands a baton to
  exactly one at a time. TLS, `errno`, `pthread_self`, mutex ownership and
  TLS destructors all just work, because the kernel sets `tpidrro_el0` per
  thread as usual and only the baton holder runs. No `mrs` rewriting is
  needed. The one obligation: a thread must never block in the kernel while
  holding the baton, so the blocking primitives are interposed and turned
  into scheduler waits.
- **Launcher disables ASLR for the child.** ASLR can be turned off only for
  a process you spawn, via `_POSIX_SPAWN_DISABLE_ASLR`, so the loader is a
  launcher that `posix_spawn`s the rewritten binary rather than an
  in-process rewriter. Stubs are nonetheless written slide-proof so a
  slid image still runs.
- **Syscalls pass through by default.** The supervisor is a real process,
  so intercepted libSystem calls forward to the real implementation unless
  they affect threads, blocking, time, randomness, addresses or signals.
  Determinism of file and network contents is out of scope; the outside
  world is treated as input.
- **New crate the repository in this repo**, sharing nothing with the VM except
  the RNG (`xoshiro256**`, copied). Rust, no dependencies beyond `libc`.
  The injected supervisor is a `cdylib` in the same crate.

## Components

### 1. Launcher (`src/launch.rs`)

- `posix_spawn` the rewritten binary with `_POSIX_SPAWN_DISABLE_ASLR` set
  in the spawn attributes and `DYLD_INSERT_LIBRARIES` pointing at the
  supervisor dylib in the child's environment.
- Because the supervisor and the guest share the process, the launcher's
  only jobs are building that environment, forwarding argv, and reporting
  the child's exit status, schedule hash and hook counts (which the
  supervisor writes to an inherited fd or shared page before exit).
- Also drive the rewriter (component 2) on demand so `derp run prog`
  rewrites, signs and launches in one step, caching the rewritten file.

### 2. Rewriter (`src/rewrite.rs`, `decode.rs`, `macho.rs`)

Function ranges come from the `LC_FUNCTION_STARTS` load command, which
Mach-O carries even when stripped; each start plus the next start (or
section end) bounds a function. No `.symtab` dependency.

We never move existing code: every hook site keeps its original address, so
indirect branches, jump tables, return addresses and function pointers keep
working with no analysis. Only branches are hooked, and a branch carries no
state to relocate beyond its target.

Decoder classes, nothing else is decoded:

| Class | Instructions | Action |
|-------|-------------|--------|
| Branch, hooked | `b`/`b.cond` backward, `bl`, `blr`, `br`, `cbz`/`cbnz` and `tbz`/`tbnz` backward | replace with `b stub` / `b.cond stub` |
| Branch, left alone | forward `b`, forward conditional, `ret` | none |
| Memory | `ldr`/`str` and byte/half/word forms, `ldur`/`stur`, `ldp`/`stp`, register-offset and literal forms, `ldar`/`stlr`, LSE atomics | candidates for sparse hooks |
| Exclusive | `ldxr`/`ldaxr` … `stxr`/`stlxr` | mark the span unhookable |
| Stack address | `add`/`sub xN, sp|x29, #k`, `mov xN, sp`, register-form adds with `sp`, excluding `mov x29, sp` / `add x29, sp, #k` | taints the function (see rule below) |
| System call | `svc` | left alone; libSystem is interposed, and program text rarely issues raw `svc` |

**Code discovery.** ARM64 is fixed-width and 4-byte aligned, and compiler
output (Clang, rustc) keeps constant pools and jump tables in data
sections, not inline in text. The rule "patch only words that decode as a
valid branch and fall inside a known function range" is conservative and
covers compiler output; inter-function padding decodes as nops or invalid
words, never as branches. Hand-written assembly with inline `.word` data in
text is out of scope, as are runtime code generators.

**Stack rule.** A memory instruction with base `sp` or `x29` is never a
candidate unless its function contains a stack-address materialization, in
which case all of that function's stack accesses are candidates.

**Sparse selection.** Each candidate memory instruction is hooked with
probability `p` (default 1/16, flag `--mem-hook-rate`), drawn from a PRNG
seeded by the run seed. `--mem-hook-rate 0` gives branch-only; `1` gives
dense.

**Mach-O output.** Read the guest, add a new segment (`__STUB`) holding the
stub text plus a small data page (the quantum counter and a pointer slot
for the scheduler entry), placed within ±128 MB of `__text` so a `b`
reaches. Patch the branch sites in `__TEXT`. Rewrite the load commands,
recompute the code-signature superblob, and ad-hoc sign
(`codesign -s -` equivalent, or emit the `LC_CODE_SIGNATURE` blob
directly). The output is a standalone executable that runs with or without
the supervisor; without it, the scheduler slot is null and stubs fall
straight through (see below).

**Stub region and slide-proofing.** Stubs are emitted from fixed templates;
the fast path never executes a flag-setting instruction. The counter lives
in `__STUB`'s data page, reached with a pc-relative `adrp`/`add`, so it
needs no fixup and one counter is shared across all threads. The scheduler
entry lives in the *dylib*, which slides independently, so the stub does
not name it directly: it loads a function pointer from a slot in `__STUB`'s
data page that the dylib's constructor fills at load. Before the dylib
loads (or if it never does) the slot is null and the expiry path skips the
call, so a rewritten binary still runs standalone.

Branch stub (unconditional site `b target` becomes `b stub`):

```
stub:   stp  x0, x1, [sp, #-16]!
        adrp x0, counter            ; counter in __STUB data page (pc-rel, no fixup)
        add  x0, x0, :lo12:counter
        ldr  x1, [x0]
        sub  x1, x1, #1
        str  x1, [x0]
        cbz  x1, expired
        ldp  x0, x1, [sp], #16
        b    target
expired: adrp x0, sched_slot         ; pointer slot filled by the dylib at load
        ldr  x0, [x0, :lo12:sched_slot]
        cbz  x0, skip                ; null when running standalone
        blr  x0                      ; scheduler_yield: saves/restores all incl. nzcv, resets counter
skip:   ldp  x0, x1, [sp], #16
        b    target
```

Variants: `b.cond` sites keep the condition at the site, the stub ends in
an unconditional `b`. `bl`/`blr` stubs load `x30` with `site + 4` from a
literal before the final `b target` / `br xN`. `cbz`/`tbz` sites likewise
keep the test at the site. Conditional branches with short reach
(`b.cond`/`cbz` ±1 MB, `tbz` ±32 KB) are handled by inverting the
condition in the stub and taking an unconditional `b` to the far target.
Memory-instruction stubs re-execute the displaced instruction after the
check (literal loads re-encoded) and `b site + 4`.

### 3. Supervisor dylib (`src/dylib.rs`)

Injected with `DYLD_INSERT_LIBRARIES`. Its constructor runs before the
guest's `main`:

- Fills the `sched_slot` pointer in the rewritten image's `__STUB` data
  page with the address of `scheduler_yield`, and seeds the counter.
- Registers the interposers (component 5) via the `__DATA,__interpose`
  section, so `write`, `pthread_create`, the blocking primitives and
  `malloc` route through us, including from inside libSystem.
- Registers the main thread with the scheduler and takes the baton.

### 4. Scheduler (`src/sched.rs`)

- Per-thread record: OS tid, a semaphore, state
  (running / runnable / blocked-on(addr) / exited), the guest entry the
  thread is parked on.
- One global `counter` (in the guest image's `__STUB` page). On expiry,
  `scheduler_yield` draws the next quantum from the seeded RNG
  (`--quantum lo..hi`, default 1000..10000 hook events), picks the next
  runnable thread with the same RNG, signals its semaphore and waits on its
  own. Only the baton holder executes guest code; the kernel's choice of
  when to run a parked thread is invisible because parked threads do
  nothing.
- Schedule trace: every switch appends (from, to, counter value) to a
  buffer; its hash is printed at exit for determinism checks.

### 5. Interposition layer (`src/interpose.rs`)

Interposed libSystem functions, each forwarding to the real implementation
unless it affects the schedule or determinism:

| Function(s) | Handling |
|-------------|----------|
| `pthread_create` | create the real pthread, but its start routine is our trampoline: register with the scheduler, park until it holds the baton, then call the guest start routine |
| `pthread_mutex_lock` / `_unlock` / `_trylock` | model ownership in the scheduler; a contended lock yields instead of entering the kernel, so the baton holder never blocks |
| `pthread_cond_wait` / `_signal` / `_broadcast` | translate to scheduler waits and wakes |
| `__ulock_wait` / `__ulock_wake`, `psynch_*` | the futex-level primitives under the pthread APIs; interposed as a backstop and turned into scheduler waits/wakes |
| `pthread_join` | wait on the target thread's scheduler state, not the kernel |
| `pthread_exit`, thread return | hand off the baton, mark exited, then real exit |
| `malloc` / `free` / `calloc` / `realloc` | a deterministic bump-or-freelist allocator with fixed addresses, so heap layout is a function of the call sequence (also sidesteps libmalloc's per-CPU magazines, whose choice depends on the running core) |
| `mmap` / `munmap` / `mprotect` | pass through with fixed, supervisor-chosen addresses so layout is deterministic |
| `arc4random`, `getentropy`, `CCRandomGenerateBytes` | seeded bytes |
| `clock_gettime`, `gettimeofday`, `mach_absolute_time`, `nanosleep` | virtual clock: advances a fixed amount per switch; sleeps yield |
| `write`, `read`, `open`, `close`, `fstat`, `ioctl`, `getpid`, `pthread_self` | pass through |
| anything else that shows up | pass through and log the name once |

A blocking pass-through call (e.g. `read` on a pipe) blocks while holding
the baton; this is a known limitation, acceptable for the test programs.

### 6. Test programs (`tests/programs/`)

1. `race.c`: two threads, N non-atomic increments each of a global; prints
   the total. Also a variant where the counter is a local in `main`
   shared by address (exercises the stack rule).
2. `mutex.c`: same with a pthread mutex; the total must always be 2N.
3. `channel.rs` (std, `aarch64-apple-darwin`): three threads, an mpsc
   channel, a `HashMap`, `println!`; deterministic output.
4. `loops.c`: sieve, matmul and a recursive function, for overhead
   measurements against the unrewritten binary.

### 7. Driver and measurements

`derp run --seed S [--mem-hook-rate R] [--quantum LO..HI] prog args…`
rewrites and signs the binary if needed, launches it, and prints the
guest's exit status, the schedule hash and hook counts to stderr.
`derp bench prog` runs native and rewritten and prints the ratio.

## Implementation Order

| Day | Deliverable |
|-----|-------------|
| 1 | Mach-O reader/writer: parse load commands and `LC_FUNCTION_STARTS`, append a `__STUB` segment, re-sign ad hoc; an unmodified static hello world round-trips through the rewriter and still runs. Launcher `posix_spawn`s it with ASLR off and the (empty) dylib injected |
| 2 | Rewriter for branch classes with function ranges, stub emission, counter; dylib constructor fills the scheduler slot; `loops.c` runs rewritten; overhead measured |
| 3 | Thread interposition with parking trampoline, baton scheduler, mutex/cond translation, thread exit/join; `mutex.c` and `channel.rs` run correctly |
| 4 | Seeded quanta, schedule trace, memory-hook selection with the stack and exclusive-section rules; `race.c` reproduces from a seed |
| 5 | Determinism hardening (deterministic allocator, mmap placement, seeded randomness, virtual clock); 100-run identical-hash check; overhead numbers for both hook modes; write-up |
| 6-7 | Slack: blocking pass-through, code-signing edge cases, anything the Rust binary turned up |

## Acceptance Criteria

- `race.c` with branch hooks only always prints 2N. With
  `--mem-hook-rate 1/16` at least one seed in twenty prints less, and that
  seed prints the same total and schedule hash on 100 consecutive runs.
  The stack-shared variant behaves the same.
- `mutex.c` and `channel.rs` produce correct output on every seed, with
  identical schedule hashes per seed.
- `channel.rs` rewrites without a crash: every function in the binary is
  scanned, and the log of skipped or unknown instruction classes is empty
  for compiler-generated code.
- The rewritten binary is accepted and executed by the kernel (ad-hoc
  signature validates) and runs standalone with the dylib absent.
- Overhead on `loops.c`: measured and reported for branch-only and for
  rates 1/16 and 1. Targets, not gates: under 1.3x and under 3x.

## Non-Goals

- Linux, dynamic-library rewriting (only the program is rewritten, not its
  dylibs), x86-64.
- Rewriting `mrs tpidrro_el0`; baton-scheduled real threads make it
  unnecessary. The `mrs` detour is kept in reserve for the rare program
  site if one turns up.
- Rewriting libSystem itself; a quantum can only expire in the program's
  own text, and interposed lock/`write` calls are the scheduling points
  inside library code.
- Sockets or record/replay of external input.
- Delivering signals to the guest.
- Programs that generate code at runtime (JITs, JS engines).
- Race detection; the only detector is the program's own output.

## Risks

- **Ad-hoc signing and page modification.** Emitting a valid
  `LC_CODE_SIGNATURE` by hand is fiddly; fall back to shelling out to
  `codesign -s -` on the rewritten file if the in-process path stalls.
- **`DYLD_INSERT_LIBRARIES` ignored.** dyld strips it for restricted
  binaries (setuid, hardened runtime, library-validation entitlement). We
  control the toolchain, so build guests without those; document the flags.
- **Rust std startup** expects things we stub
  (`available_parallelism` via `sysctl`/`sched`, TLV setup): log and stub
  as found.
- **libmalloc per-CPU magazines** make heap addresses depend on the running
  core; the deterministic allocator removes the dependency and is required,
  not optional.
- **A hooked memory instruction inside a sequence the compiler assumed
  atomic** beyond LL/SC (none known on ARM64; the exclusive-section rule is
  the only case).
- **The child trampoline** runs on the guest-provided stack; it must stay
  within a few hundred bytes.
