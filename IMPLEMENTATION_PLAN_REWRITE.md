# Native Binary Rewriting: One-Week Experiment

## Overview

Instead of compiling programs to DVM bytecode, run real ARM64 executables
under a supervisor that owns their system calls and their thread schedule.
The executable's text is patched in place: every branch becomes a branch to
a stub that counts down a quantum and enters the scheduler when it expires;
a pseudo-random subset of loads and stores get the same treatment so
switches can land inside basic blocks. Threads are real OS threads that run
one at a time, handing a baton between them at those switch points.

The experiment answers three questions with numbers:

1. How reliable is in-place rewriting on real compiler output (a static
   Rust binary with std)?
2. What does the hook overhead cost, with branch hooks only and with sparse
   memory hooks?
3. Does the scheduler reproduce a two-thread lost-update race from a seed,
   and never reproduce it with branch hooks only?

## Decisions

- **Target: aarch64 Linux, static executables.** Linux offers Syscall User
  Dispatch (no rewriting needed to catch `svc`), writable text pages, user
  writes to `tpidr_el0`, and no code signing. Static musl binaries (C via
  `musl-gcc`, Rust via `aarch64-unknown-linux-musl`) avoid the dynamic
  loader entirely. Development runs in an arm64 Linux container on the Mac
  (`docker run --platform linux/arm64`; the kernel must be 5.11+ for SUD).
  macOS is a follow-up: the same design with dyld interposition instead of
  SUD and a rewritten, re-signed Mach-O instead of load-time patching.
- **Rewriting happens at load time, in memory.** The loader maps the ELF,
  patches the text, and jumps to the entry point. No file is written. The
  set of hooked memory instructions is drawn from the run's seed, so a seed
  identifies both the hook placement and the schedule.
- **Syscalls pass through by default.** The supervisor is a real process, so
  most syscalls are forwarded to the kernel unchanged. Only those that
  affect threads, blocking, time, randomness, addresses and signals are
  emulated. Determinism of file and network contents is out of scope; the
  outside world is treated as input.
- **New crate the repository in this repo**, sharing nothing with the VM except
  the RNG (`xoshiro256**`, copied). Rust, no dependencies beyond `libc`.

## Components

### 1. Loader (`src/loader.rs`)

- Parse the static ELF (ET_EXEC or ET_DYN/static-pie), map PT_LOAD segments
  at their addresses (PIE at a fixed base), zero-fill bss.
- Build the initial stack: argv, envp, auxv with AT_PHDR, AT_PHNUM,
  AT_PAGESZ, AT_ENTRY, AT_RANDOM (16 seeded bytes), AT_SECURE=0, AT_UID
  etc. **No AT_SYSINFO_EHDR**: without a vDSO, musl and Rust's libc calls
  fall back to real `svc` for `clock_gettime`, so time is intercepted like
  everything else.
- Enable SUD with `prctl(PR_SET_SYSCALL_USER_DISPATCH, PR_SYS_DISPATCH_ON,
  start, len, &selector)`, where `[start, len)` is the loader's own text.
  Install the SIGSYS handler on a per-thread `sigaltstack`.
- Jump to the entry with the guest stack. The loader is built as a
  non-PIE binary at a high base so it never overlaps a guest image.

### 2. Rewriter (`src/rewrite.rs`, `decode.rs`)

Function ranges come from `.symtab` (STT_FUNC with sizes); the experiment
requires unstripped binaries. `.eh_frame` ranges are a follow-up.

Decoder classes, nothing else is decoded:

| Class | Instructions | Action |
|-------|-------------|--------|
| Branch, hooked | `b`/`b.cond` backward, `bl`, `blr`, `br`, `cbz`/`cbnz` and `tbz`/`tbnz` backward | replace with `b stub` / `b.cond stub` |
| Branch, left alone | forward `b`, forward conditional, `ret` | none |
| Memory | `ldr`/`str` and byte/half/word forms, `ldur`/`stur`, `ldp`/`stp`, register-offset and literal forms, `ldar`/`stlr`, LSE atomics | candidates for sparse hooks |
| Exclusive | `ldxr`/`ldaxr` … `stxr`/`stlxr` | mark the span unhookable |
| Stack address | `add`/`sub xN, sp|x29, #k`, `mov xN, sp`, register-form adds with `sp`, excluding `mov x29, sp` / `add x29, sp, #k` | taints the function (see rule below) |
| System call | `svc` | left alone; SUD catches it |

**Stack rule.** A memory instruction with base `sp` or `x29` is never a
candidate unless its function contains a stack-address materialization, in
which case all of that function's stack accesses are candidates.

**Sparse selection.** Each candidate memory instruction is hooked with
probability `p` (default 1/16, flag `--mem-hook-rate`), drawn from a PRNG
seeded by the run seed. `--mem-hook-rate 0` gives branch-only; `1` gives
dense.

**Stub region.** One `mmap` within ±128 MB of the text (hint: just above
the highest PT_LOAD). Stubs are emitted from fixed templates; the fast path
never executes a flag-setting instruction.

Branch stub (unconditional site `b target` becomes `b stub`):

```
stub:   stp  x0, x1, [sp, #-16]!
        adrp x0, counter
        add  x0, x0, :lo12:counter
        ldr  x1, [x0]
        sub  x1, x1, #1
        str  x1, [x0]
        cbz  x1, expired
        ldp  x0, x1, [sp], #16
        b    target
expired: bl  scheduler_yield          ; saves/restores everything incl. nzcv, sets counter
        ldp  x0, x1, [sp], #16
        b    target
```

Variants: `b.cond` sites keep the condition at the site, the stub ends in
an unconditional `b`. `bl`/`blr` stubs load `x30` with `site + 4` from a
literal before the final `b target` / `br xN`. `cbz`/`tbz` sites likewise
keep the test at the site. Memory-instruction stubs re-execute the
displaced instruction after the check (literal loads re-encoded) and
`b site + 4`.

### 3. Scheduler (`src/sched.rs`)

- Per-thread record: OS tid, a semaphore (futex-based), state
  (running / runnable / blocked-on-futex(addr) / exited), the guest's
  `clear_child_tid` pointer.
- One global `counter`. On expiry, `scheduler_yield` draws the next quantum
  from the seeded RNG (`--quantum lo..hi`, default 1000..10000 hook events),
  picks the next runnable thread with the same RNG, signals it and waits on
  its own semaphore. Only the baton holder executes guest code.
- Schedule trace: every switch appends (from, to, counter value) to a
  buffer; its hash is printed at exit for determinism checks.

### 4. Syscall layer (`src/syscall.rs`)

SIGSYS handler reads the syscall number and arguments from the ucontext
and dispatches:

| Syscall | Handling |
|---------|----------|
| `clone`/`clone3` with CLONE_THREAD | perform the real clone from the supervisor with the guest's stack, TLS and tid pointers; the child starts in a supervisor trampoline that registers itself, parks until it holds the baton, then materializes the guest's registers with x0 = 0 and continues at the guest pc |
| `futex` WAIT / WAIT_BITSET | if `*addr != val` return EAGAIN; else mark blocked on addr, yield; on wake return 0. Timeout: if no thread is runnable, return ETIMEDOUT (virtual time) |
| `futex` WAKE / WAKE_BITSET | make up to n waiters runnable, return count |
| `exit` (thread) | write 0 to clear_child_tid, futex-wake it, mark exited, hand off baton, real exit |
| `exit_group` | flush trace, real exit_group |
| `set_tid_address`, `set_robust_list`, `rseq`, `sched_getaffinity`, `prlimit64`, `membarrier` | emulate trivially |
| `rt_sigaction`, `rt_sigprocmask`, `sigaltstack` | record and succeed without installing; SIGSYS must never be blocked |
| `mmap`, `munmap`, `mprotect`, `brk`, `madvise` | pass through with `MAP_FIXED_NOREPLACE` at supervisor-chosen addresses so the layout is a function of the call sequence |
| `getrandom` | seeded bytes |
| `clock_gettime`, `gettimeofday`, `nanosleep`, `clock_nanosleep` | virtual clock: advances by a fixed amount per switch; sleeps yield |
| `write`, `read`, `openat`, `close`, `fstat`, `ioctl`, `readlinkat`, `getpid`, `gettid`, `uname`, … | pass through |
| anything else | pass through and log the number once |

Asynchronous signals are blocked on every guest thread. A blocking
pass-through call (e.g. `read` on a pipe) blocks while holding the baton;
this is a known limitation, acceptable for the test programs.

### 5. Test programs (`tests/programs/`)

1. `race.c`: two threads, N non-atomic increments each of a global; prints
   the total. Also a variant where the counter is a local in `main`
   shared by address (exercises the stack rule).
2. `mutex.c`: same with a pthread mutex; the total must always be 2N.
3. `channel.rs` (std, static musl): three threads, an mpsc channel, a
   `HashMap`, `println!`; deterministic output.
4. `loops.c`: sieve, matmul and a recursive function, for overhead
   measurements against the unrewritten binary.

### 6. Driver and measurements

`rewrite run --seed S [--mem-hook-rate R] [--quantum LO..HI] prog args…`
prints the guest's exit status, the schedule hash and hook counts to
stderr. `rewrite bench prog` runs native and rewritten and prints the
ratio.

## Implementation Order

| Day | Deliverable |
|-----|-------------|
| 1 | Container with musl toolchains; loader runs an unmodified static hello world with SUD on and every syscall passed through |
| 2 | Rewriter for branch classes with function ranges, stub emission, counter; `loops.c` runs rewritten; overhead measured |
| 3 | Thread interception with parking child, baton scheduler, futex, thread exit; `mutex.c` and `channel.rs` run correctly |
| 4 | Seeded quanta, schedule trace, memory-hook selection with the stack and exclusive-section rules; `race.c` reproduces from a seed |
| 5 | Determinism hardening (mmap placement, getrandom, virtual clock, AT_RANDOM); 100-run identical-hash check; overhead numbers for both hook modes; write-up |
| 6-7 | Slack: `.eh_frame` ranges, blocking pass-through, anything the Rust binary turned up |

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
- Overhead on `loops.c`: measured and reported for branch-only and for
  rates 1/16 and 1. Targets, not gates: under 1.3x and under 3x.

## Non-Goals

- macOS, dynamic linking, stripped binaries, x86-64.
- Sockets or record/replay of external input.
- Delivering signals to the guest.
- Programs that generate code at runtime.
- Race detection; the only detector is the program's own output.

## Risks

- SUD unavailable in the container's kernel: fall back to seccomp
  `SECCOMP_RET_TRAP`, same handler.
- Rust std's startup expects things we stub (`sigaltstack` guard pages,
  `sched_getaffinity` for `available_parallelism`): log and stub as found.
- A hooked memory instruction inside a sequence the compiler assumed
  atomic beyond LL/SC (none known on ARM64; the exclusive-section rule is
  the only case).
- The child trampoline runs on the guest-provided stack below the initial
  `sp`; it must not exceed a few hundred bytes.
