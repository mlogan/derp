# Multi-Process Deterministic Runs: Twelve-Day Experiment

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
   processes communicate through pipes, shared files, and sockets that the
   supervisor implements itself (the seam a network simulator will later
   plug into)?
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
- **Sockets between guests are virtual.** The supervisor implements
  `AF_INET` and `AF_UNIX` stream and datagram sockets in the shared state:
  ring buffers per direction, listen queues, a port table. No packet touches
  the kernel's network stack, so there is nothing asynchronous to settle
  (loopback TCP in the kernel is delivered on an input thread, which would
  have made a peer's non-blocking `recv` timing-dependent). This is also
  the seam for the network simulator that will follow: every byte between
  guests already passes through one `deliver` function, which in this
  experiment delivers instantly and in order. The simulator replaces that
  policy with latency, loss, partitions and reordering driven by the seeded
  RNG and the virtual clock, without touching the socket API layer.
- **A virtual socket still has a real fd.** `socket()` returns a real
  placeholder descriptor so fd numbers, `dup`, `close`, `fork` inheritance
  and `select` bitmaps stay coherent; the shared state maps (process, fd) to
  the virtual socket and refcounts it.
- **Sockets to addresses no guest has bound** are real: a loopback address
  with no listener gets ECONNREFUSED; any other address passes through to
  the kernel, is logged once, and is treated as input.
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

### 5. I/O interposition for kernel objects (`supervisor/src/io.rs`)

Pipes, files and pass-through sockets. Each call first asks the fd table
whether the descriptor is virtual (component 6); if so it is routed there.

| Call | Handling |
|---|---|
| `read`, `readv` | regular file: pass through. Pipe or real socket: non-blocking try; on EAGAIN park as an I/O waiter for (fd, readable), retry when woken |
| `write`, `writev` | non-blocking try; on EAGAIN park for (fd, writable); after success wake all I/O waiters |
| `close`, `unlink`, `rename`, lock release | pass through, then wake all I/O waiters |
| `poll`, `select`, `kevent` | readiness of virtual fds comes from shared state, of real fds from the same call with zero timeout; if nothing is ready and the timeout is nonzero, park with a virtual-time deadline and re-evaluate when woken |
| `flock`, `fcntl(F_SETLKW)` | try the non-blocking form; on EWOULDBLOCK park for (fd, lock) and retry when woken |
| `dup`, `dup2`, `fcntl(F_DUPFD)`, `fork`, `execve` | keep the (process, fd) → virtual socket map and refcounts in step with the kernel's fd table |
| `fsync`, `open`, `stat`, `lseek`, `mmap` of files | pass through; `open` logs paths outside the scratch directory once |

Every call in this table and the next also decrements the shared quantum
counter and yields if it expires.

### 6. Virtual network (`supervisor/src/net.rs`)

State in the shared file: a socket table (`{ kind, state, local, peer,
rx: ring, backlog, options, refs }`), a port table per address family, and
an in-flight queue of `{ deliver_at, dst_socket, bytes | fin | datagram }`.

| Call | Handling |
|---|---|
| `socket` | allocate a virtual socket and a real placeholder fd |
| `bind`, `listen` | port table entry (ephemeral ports handed out in order); UNIX-domain paths are names in the table, no filesystem node |
| `connect` | find the listener in the port table: queue a connection on its backlog, wake accept waiters, park until accepted (or return EINPROGRESS when non-blocking). No listener: ECONNREFUSED for loopback, otherwise fall back to a real socket |
| `accept` | pop the backlog or park for readable |
| `send`, `sendto`, `sendmsg`, `write`, `writev` | hand the bytes to `deliver(src, dst, payload)`; stream sockets block (or EAGAIN) when the peer's ring is full, which gives real backpressure |
| `recv`, `recvfrom`, `recvmsg`, `read`, `readv` | copy from the ring, or park for readable; zero-length read after the peer's FIN |
| `shutdown`, `close` | FIN through `deliver`; last reference frees the socket; writes to a closed peer return EPIPE |
| `getsockname`, `getpeername`, `getsockopt`, `setsockopt`, `ioctl(FIONREAD)`, `fcntl(O_NONBLOCK)` | answered from socket state; `SO_RCVTIMEO`/`SO_SNDTIMEO` become virtual-time deadlines; options with no meaning here (`TCP_NODELAY`, `SO_REUSEADDR`, keepalive) are accepted and recorded |

`deliver` is the single entry point for traffic. In this experiment it
appends to the destination immediately. Its signature already carries
source, destination and the virtual clock, and the scheduler already drains
the in-flight queue when it advances the clock, so a simulator only has to
choose `deliver_at` (latency), whether to enqueue at all (loss, partition)
and queue order for datagrams (reordering). A fixed `--net-latency` option
exercises that path in the tests.

`kevent` needs an emulation layer for virtual fds: registrations on a kqueue
are recorded per (process, kq fd), and readiness is synthesized from socket
state. Level-triggered, `EV_CLEAR` and `EV_ONESHOT` are in scope; anything
else is logged.

### 7. Virtual clock and timers (`supervisor/src/clock.rs`)

Moves from per-process atomics to the shared state. `pick()` gains a
deadline check: when nothing is runnable, advance the clock to the earliest
deadline (a timed waiter's or an in-flight network item's) and act on it;
when something is runnable, deadlines that have passed are handled before
the choice.

### 8. Header room in the rewriter (`src/macho.rs`)

So default-linked binaries from the user's toolchain need no relinking: emit
the `__STUB` segment without a section header (72 bytes), put the counter
pointer slot and magic in the zero-fill slack at the end of `__DATA` when at
least 32 bytes are free, and drop `LC_UUID`, `LC_SOURCE_VERSION` and an
empty `LC_DATA_IN_CODE` when space is still short. Keep the
`-Wl,-headerpad` error message for the rare binary that still does not fit.

### 9. Test programs (`tests/programs/`)

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
6. `udp_ping.c`: two processes exchange numbered datagrams and print what
   arrived, in order of arrival.
7. `poll_server.c`: a single-threaded server multiplexing several clients
   with `poll`, and the same with `kevent`, using read timeouts.

## Implementation Order

| Day | Deliverable |
|---|---|
| 1 | Shared scheduler state and cross-process baton; launcher starts N initial guests; `mutex.c` and `channel.rs` still pass; two `loops` processes alternate with a stable run-wide hash |
| 2 | Counter relocation into shared state; header-room changes in the rewriter; default-linked hello world rewrites without `-headerpad` |
| 3 | Virtual pids, `posix_spawn`/`fork`/`execve`/`waitpid` interposition, report aggregation; `pipeline.c` spawns its stages (pipes still pass through) |
| 4 | Readiness waits for pipes; fd table bookkeeping across `dup`/`fork`/`execve`; `pipeline.c` correct with a stable hash |
| 5 | Virtual stream sockets: socket table, port table, `connect`/`accept`/`send`/`recv`/`close`, backpressure; `tcp_echo.c` over TCP and `--unix` |
| 6 | Socket options, non-blocking mode, `shutdown`, datagram sockets; `udp_ping.c`; `deliver` with the in-flight queue |
| 7 | Shared virtual clock, deadline-ordered timed waits; `poll`/`select` over mixed real and virtual fds; sleeps become timed waits |
| 8 | `kevent` emulation for virtual fds; `poll_server.c` in both forms; fixed `--net-latency` through the in-flight queue |
| 9 | `counter_file.c` and `shared_map.c`: races reproduce from a seed; I/O calls as hook events; 100-run checks |
| 10 | `net.rs`; switch-cost and overhead measurements; quantum defaults revisited |
| 11-12 | Slack: whatever the Rust program turned up, socket API gaps found by the tests, write-up in `docs/MULTIPROC_RESULTS.md` |

## Acceptance Criteria

- `pipeline.c`, `tcp_echo.c` (TCP and UNIX), `udp_ping.c` and
  `poll_server.c` (both forms) produce correct output on every seed, with a
  run-wide schedule hash identical on 100 consecutive runs per seed and
  different across seeds.
- No traffic between guests reaches the kernel: the report shows every
  connection and byte count through the virtual network, and no guest holds
  a bound or connected kernel socket during the tests.
- With `--net-latency` set to a fixed value, outputs stay correct, the
  schedule changes, and the 100-run check still passes: the simulator seam
  works end to end.
- `counter_file.c --flock` always prints N·K. Without `--flock`, at least
  one seed in twenty prints less with branch hooks only (I/O calls are hook
  events), and that seed prints the same value and hash on 100 runs.
- `shared_map.c`: same as `race.c` today: never wrong with branch hooks
  only; wrong for some seed at `--mem-hook-rate 1/16`, reproducibly.
- `net.rs` runs correctly with a stable hash.
- A default-linked (no `-headerpad`) hello world rewrites and runs.
- Numbers reported: switch cost in-process vs cross-process; overhead on
  `loops.c` with the relocated counter; virtual socket throughput against
  kernel loopback for `tcp_echo.c`.

## Non-Goals

- System utilities and platform binaries.
- Signals other than SIGTERM/SIGKILL to parked processes.
- The network simulator itself: latency distributions, loss, partitions,
  bandwidth limits, datagram reordering, per-process virtual hosts and
  addresses. This experiment builds the seam (`deliver`, the in-flight
  queue, the clock hookup) and proves it with a fixed latency only.
- Sockets to hosts outside the run (they pass through as input); DNS.
- Descriptor passing (`SCM_RIGHTS`), raw sockets, `AF_INET6`, multicast,
  out-of-band data, `sendfile`.
- Filesystem virtualization or sandboxing beyond the scratch cwd.
- Virtual machines or containers.
- Deterministic `readdir` order beyond what APFS gives for a fixed set of
  names.

## Risks

- **Socket API surface.** Real programs probe options and corner cases
  (`SO_ERROR` after a non-blocking connect, `MSG_PEEK`, `MSG_WAITALL`,
  half-close, `EPIPE` versus `ECONNRESET`). Each gap shows up as a test
  program misbehaving; unknown options and flags are logged by name so the
  list of what to add is explicit.
- **fd bookkeeping.** A virtual socket's identity lives outside the kernel,
  so every path that copies or drops descriptors (`dup2` over a virtual fd,
  `fork`, close-on-exec, process death without `close`) must update the
  refcounts, or peers never see EOF. Process exit sweeps the table.
- **`kevent` semantics.** Emulating edge-triggered delivery and the
  interaction of virtual and real registrations on one kqueue is the most
  intricate part; runtimes like tokio depend on it. It is scheduled late
  and is the first thing to cut to `poll`-only if time runs short.
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
