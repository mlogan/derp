# Multi-Process Deterministic Runs: Two-Week Experiment

## Overview

Extend the single-process rewriting experiment (`IMPLEMENTATION_PLAN_REWRITE.md`,
results in `docs/REWRITE_RESULTS.md`) to several guest processes that talk to
each other through files, pipes and sockets. The launcher becomes the owner
of one scheduler shared by every guest: exactly one thread in the whole run
holds the baton, and it is handed over at quantum expiry, at blocking calls,
and now at blocking I/O. Everything runs from the command line on the host:
no images, no guest kernels, no relinking of the user's binaries beyond
what the rewriter already needs.

The experiment answers three questions with numbers:

1. Is the schedule of a multi-process run a function of the seed when the
   processes communicate through the kernel (pipes, loopback TCP, UNIX
   sockets, shared files)?
2. Does the cross-process baton cost enough per switch to change the quantum
   defaults?
3. Does a seed reproduce a cross-process race (lost update on a shared file
   or a shared mapping) the way it reproduces the two-thread race today?

## Decisions

- **Guests are our binaries only.** No `/bin/sh`, no coreutils: every
  process in a run is rewritten and has the supervisor injected. Platform
  binaries are out of scope for this experiment.
- **Scheduler state in a shared-memory file.** The launcher creates it and
  every guest inherits it. It holds the runnable set keyed by
  (process, thread), the RNG, the virtual clock, the trace hash and the
  wait keys, under a spinlock (holders never sleep while holding it).
  Threads park on `os_sync_wait_on_address` with the shared flag. Switch
  cost stays near the in-process cost; no coordinator on the hot path.
- **The launcher handles lifecycle only**, over a UNIX socket per guest:
  registering a process and its threads, rewriting a binary on demand for
  a spawned child, reaping, and collecting reports. It starts the initial
  processes named on the command line; guests may spawn more.
- **Wait keys are namespaced by process.** Addresses collide across guests
  (heap layout is identical by construction), so every key is
  (pid, address) or (pid, fd).
- **Blocking I/O becomes a readiness wait.** A call that would block tries
  the non-blocking form first; if not ready the thread parks as an I/O
  waiter. Because only the baton holder runs, kernel object state changes
  only when a guest acts, so waking every I/O waiter after any write,
  close, connect, shutdown, accept or unlink and letting them re-check is
  exact, not a heuristic. The one exception is below.
- **Loopback TCP settles asynchronously in the kernel.** Delivery over
  `lo0` happens on a kernel input thread, so a `send` can return before the
  peer's receive buffer has the data, and a peer's non-blocking `recv` a
  moment later could see EAGAIN or data depending on kernel timing. The
  sender therefore does not release the baton until the kernel reports the
  change delivered: `FIONWRITE` reaching zero after a send, `POLLOUT` after
  a connect. `close` and `shutdown` cannot be observed from the closing
  side and use a bounded wait. UNIX-domain sockets and pipes deliver
  synchronously and need none of this. If the settling rule proves leaky in
  the 100-run check, the experiment falls back to UNIX-domain sockets and
  records the TCP result as a finding.
- **I/O syscalls count as hook events.** Each interposed I/O call decrements
  the shared quantum counter like a stub does, so a quantum can expire at an
  I/O boundary. A read-modify-write on a file (read, compute, write) then
  has switch points inside it even with branch hooks only.
- **Virtual pids** are assigned in spawn order and translated in `getpid`,
  `getppid`, `kill` and `waitpid`, so pids in output and temp-file names are
  repeatable.
- **One virtual clock for the run**, in the shared state, advancing per read
  and per switch. Timed waits (`poll`, `kevent`, `select`, `nanosleep`,
  `pthread_cond_timedwait`, `os_sync_wait_on_address_with_timeout`) expire
  in virtual-time order, replacing the "release when idle" rule.
- **Files are not virtualized.** The launcher creates a scratch directory
  per run, sets it as cwd and `TMPDIR`, and passes it to the guests. Paths
  outside it are allowed and logged once; their contents are input.

## Components

### 1. Shared scheduler (`supervisor/src/shared.rs`, replaces `sched.rs` state)

Fixed-size `#[repr(C)]` state in a file mapped at the same address in every
guest (fixed address, `MAP_SHARED`; the launcher checks placement):

- `lock`: spinlock word.
- `threads[MAX_THREADS]`: `{ pid, tid, state, key: (pid, u64), timed_until,
  signaled, park_word }`; `park_word` is what the thread waits on with
  `os_sync_wait_on_address` (shared) and what the baton giver bumps.
- `rng`, `issued`, `switches`, `expiries`, `trace_hash`, `clock_ns`,
  `quantum_lo/hi`, `counter` (the quantum counter moves here from `__STUBD`;
  the stub's `adrp` target becomes a slot holding a pointer to it — see 4).
- Per-process records: virtual pid, real pid, state, exit status, parent.
- `cond_waiters`: fixed-capacity queues keyed by (pid, address).
- `io_waiters`: threads parked for readiness, with (pid, fd, events).

`yield_baton` works as today but over shared state: pick the next runnable
thread from any process, bump its `park_word`, wake it with
`os_sync_wake_by_address_any` (shared), park on our own word.

### 2. Launcher (`src/launch.rs`, `src/coord.rs`)

- Creates the scratch directory, the shared file, the RNG seed and the
  quantum range; spawns each initial process with ASLR off and the dylib
  injected, passing the shared-file path and its socket over inherited fds.
- Socket protocol (length-prefixed, one message type per line of `enum`):
  `Register { real_pid }` → `{ vpid }`, `Spawn { path, argv, env }` →
  `{ rewritten_path }` (rewrite cache keyed by path, mtime, seed, rate),
  `Exited { vpid, status }`, `Report { text }`.
- Reaps children with `waitpid`, keeps the (vpid, real pid) map, and prints
  one aggregated report: per-process hooks, switches, and the single
  run-wide schedule hash.
- `rewrite run --seed S [--mem-hook-rate R] prog1 args… [-- prog2 args…]`
  launches several initial processes; `rewrite repeat` compares the
  aggregated report and every process's stdout (each redirected to a file
  in the scratch directory).

### 3. Process lifecycle (`supervisor/src/process.rs`)

| Call | Handling |
|---|---|
| `posix_spawn`, `posix_spawnp` | ask the launcher for the rewritten path, inject the environment (dylib, shared file, socket), register the child before it runs so it starts parked; the child's first thread joins the runnable set |
| `fork` | at-fork child hook: drop the copied thread records except the caller's, register as a new process, keep the baton (the parent parks) |
| `execve` | rewritten path substitution and environment injection; the process keeps its vpid |
| `exit`, `_exit` | report, mark the process exited, hand the baton on |
| `waitpid`, `wait4` | scheduler wait on the child's exit event, then a pass-through reap; translate vpid ↔ real pid |
| `getpid`, `getppid`, `kill` | vpid translation; `kill` with SIGTERM/SIGKILL is delivered while the target is parked and its threads are removed; other signals logged and dropped |

### 4. Stub counter relocation

The quantum counter must be shared by all processes, so the `__STUBD` page
gains a pointer slot to the counter and the stub does one more load
(`ldr x0, [x0, #COUNTER_PTR]`) before the decrement. Standalone runs (no
dylib) point the slot at a local word in `__STUBD` at rewrite time so the
binary still works with the dylib absent. Measured cost: one dependent load
per hook; recorded in the overhead table.

### 5. I/O interposition (`supervisor/src/io.rs`)

| Call | Handling |
|---|---|
| `read`, `recv`, `recvfrom`, `recvmsg`, `readv` | if the fd is a regular file: pass through. Otherwise try with `MSG_DONTWAIT` / `O_NONBLOCK`; on EAGAIN park as an I/O waiter for (fd, readable), retry when woken |
| `write`, `send`, `sendto`, `sendmsg`, `writev` | non-blocking try; on EAGAIN park for (fd, writable). After success on a socket: settling rule (`FIONWRITE` → 0). Then wake all I/O waiters |
| `accept` | non-blocking try; park for readable. After success: wake all |
| `connect` | non-blocking connect; park for writable until `SO_ERROR` is known; settle on `POLLOUT`; wake all |
| `close`, `shutdown`, `unlink`, `rename`, `flock`/`fcntl(F_SETLK)` release | pass through, then bounded settle for sockets, then wake all |
| `poll`, `select`, `kevent` | call with zero timeout; if nothing is ready and the timeout is nonzero, park as an I/O waiter with a virtual-time deadline; on wake re-issue with zero timeout |
| `flock`, `fcntl(F_SETLKW)` | try the non-blocking form; on EWOULDBLOCK park for (fd, lock) and retry when woken |
| `fsync`, `open`, `stat`, `lseek`, `mmap` of files | pass through; `open` logs paths outside the scratch directory once |

Every call in this table also decrements the shared quantum counter and
yields if it expires.

### 6. Virtual clock and timers (`supervisor/src/clock.rs`)

Moves from per-process atomics to the shared state. `pick()` gains a
deadline check: when nothing is runnable, advance the clock to the earliest
timed waiter's deadline and release it; when something is runnable, timed
waiters whose deadline has passed are made runnable before the choice.

### 7. Header room in the rewriter (`src/macho.rs`)

So default-linked binaries from the user's toolchain need no relinking: emit
the `__STUB` segment without a section header (72 bytes), put the counter
pointer slot and magic in the zero-fill slack at the end of `__DATA` when at
least 32 bytes are free, and drop `LC_UUID`, `LC_SOURCE_VERSION` and an
empty `LC_DATA_IN_CODE` when space is still short. Keep the
`-Wl,-headerpad` error message for the rare binary that still does not fit.

### 8. Test programs (`tests/programs/`)

1. `pipeline.c`: three processes connected by pipes (`producer | filter |
   consumer`), spawned by a parent with `posix_spawn`; the consumer prints a
   checksum.
2. `tcp_echo.c`: a server that accepts N connections and echoes with a
   per-connection counter, and a client that opens N connections and
   prints the replies; also runnable over a UNIX-domain socket (`--unix`).
3. `counter_file.c`: N processes each add K to a counter kept in a file by
   read, add, write; a `--flock` variant holds the file lock across the
   update. Prints the final value.
4. `shared_map.c`: two processes increment a counter in an `mmap`
   `MAP_SHARED` file with no synchronization (the cross-process version of
   `race.c`).
5. `net.rs`: Rust `std::net` `TcpListener`/`TcpStream` echo with
   `std::process::Command` spawning the client.

## Implementation Order

| Day | Deliverable |
|---|---|
| 1 | Shared scheduler state and cross-process baton; launcher starts N initial guests; `mutex.c` and `channel.rs` still pass; two `loops` processes alternate with a stable run-wide hash |
| 2 | Counter relocation into shared state; header-room changes in the rewriter; default-linked hello world rewrites without `-headerpad` |
| 3 | Virtual pids, `posix_spawn`/`fork`/`execve`/`waitpid` interposition, report aggregation; `pipeline.c` spawns its stages (pipes still pass through) |
| 4 | Readiness waits for pipes and UNIX sockets; `pipeline.c` and `tcp_echo --unix` correct with a stable hash |
| 5 | Loopback TCP settling rules; `tcp_echo.c` over TCP; 100-run check |
| 6 | Shared virtual clock, deadline-ordered timed waits, `poll`/`kevent` timeouts; sleeps become timed waits |
| 7 | `counter_file.c` and `shared_map.c`: races reproduce from a seed; I/O calls as hook events; 100-run checks |
| 8 | `net.rs`; switch-cost and overhead measurements; quantum defaults revisited |
| 9-10 | Slack: settling-rule tuning, whatever the Rust program turned up, write-up in `docs/MULTIPROC_RESULTS.md` |

## Acceptance Criteria

- `pipeline.c` and `tcp_echo.c` (TCP and UNIX) produce correct output on
  every seed, with a run-wide schedule hash identical on 100 consecutive
  runs per seed and different across seeds.
- `counter_file.c --flock` always prints N·K. Without `--flock`, at least
  one seed in twenty prints less with branch hooks only (I/O calls are hook
  events), and that seed prints the same value and hash on 100 runs.
- `shared_map.c`: same as `race.c` today: never wrong with branch hooks
  only; wrong for some seed at `--mem-hook-rate 1/16`, reproducibly.
- `net.rs` runs correctly with a stable hash.
- A default-linked (no `-headerpad`) hello world rewrites and runs.
- Numbers reported: switch cost in-process vs cross-process; overhead on
  `loops.c` with the relocated counter; TCP settling wait per send.

## Non-Goals

- System utilities and platform binaries.
- Signals other than SIGTERM/SIGKILL to parked processes.
- Sockets to hosts outside the run; DNS.
- Filesystem virtualization or sandboxing beyond the scratch cwd.
- Virtual machines or containers.
- Deterministic `readdir` order beyond what APFS gives for a fixed set of
  names.

## Risks

- **TCP settling.** The `FIONWRITE` rule covers data; FIN and RST delivery
  after `close`/`shutdown` are unobservable from the closer and use a
  bounded wait, which is a timing assumption. The 100-run check is the
  judge; UNIX-domain sockets are the fallback.
- **Kernel-woken waits inside libSystem** (the `pthread_join` lesson) will
  recur for I/O paths we have not seen; pass-through mode is the tool, and
  each case gets logged.
- **Shared-state capacity.** Fixed arrays for threads, waiters and processes
  are simple but bound the run; sized generously (1,024 threads, 256
  processes) and asserted.
- **Fixed mapping address for the shared file** must be free in every
  guest; the launcher verifies after each registration and fails loudly.
- **Counter relocation overhead** adds a load to every hook; if it costs
  more than a few percent, keep a per-process counter and make the
  scheduler reconcile hook totals at switch time instead.
