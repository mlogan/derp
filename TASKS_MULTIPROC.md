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

## Day 2 — Counter relocation, header room ✅ (design differs from the plan)

What the plan assumed and what turned out to be true:

- The plan puts the counter pointer slot in `__DATA` slack. A
  default-linked C hello world has **no `__DATA` segment**, only
  `__DATA_CONST`, which dyld makes read-only.
- Header room in that binary is 32 bytes. Dropping the old signature and
  the optional commands frees at most 80 more, and `codesign` needs 16
  back. One sectionless segment (72 bytes) fits; two do not.
- **`LC_UUID` cannot be dropped**: dyld on macOS 26 aborts with "missing
  LC_UUID load command". The drop order is an empty `LC_DATA_IN_CODE`,
  `LC_SOURCE_VERSION`, then `LC_FUNCTION_STARTS` (tool-only; its bytes
  stay orphaned in `__LINKEDIT`). Roomy binaries keep everything.

So the image gets one segment, `__STUB`, read-execute, with no section
header, and no writable page at all:

- The stubs address a **fixed region at `0x78_0000_0000`** set up by the
  supervisor: one private page per process (scheduler slot at offset 0),
  then the shared state, whose `counter` sits at region offset `0x4010`.
  A stub reaches both with `movz x0, #0x78, lsl #32`; the expired path
  reuses x0. No `adrp`, no pointer slot, no extra load.
- Per-hook cost is unchanged. A/B on `loops 3` the same afternoon: old
  local-counter stub 1.61x and 1.66x, new stub 1.61x and 1.67x. The plan's
  "counter relocation overhead" risk is retired.
- **Consequence: a rewritten binary does not run without the dylib**
  (it would fault on the unmapped region). The plan wanted standalone
  binaries to keep working. `--no-supervisor` and `bench` now inject the
  dylib with `REWRITE_PASSIVE=1`: it maps the region, registers no thread,
  and every interposer passes through. Startup cost of that is under 1 ms.
- The site counts, seed and magic (`RWST002`) moved to a 32-byte header
  at the start of `__STUB`. Cache files are tagged `.rw2-`.
- The supervisor reserves the whole region with one fixed
  `mach_vm_allocate` and maps over it; failure is fatal in the guest
  because the stubs hard-code the address.
- Hooks are still attributed per process (each process settles what it
  consumed when it gives the baton away).
- Tests: a default-linked hello rewrites, signs and runs with hooks
  counted; a roomy binary keeps its optional commands. A default-linked
  Rust `channel.rs` also runs (checked by hand, 7,287 sites).
- The test helpers still link guests with `-Wl,-headerpad,0x1000`; it is
  no longer needed.

Overhead on `loops 3` today (release build):

| Mode | Time | vs native |
|---|---|---|
| native | 0.252 s | 1.00x |
| rewritten, passive | 0.420 s | 1.66x |
| supervised, branch hooks | 0.43 s | 1.71x |
| supervised, rate 1/16 | 0.55 s | 2.2x |
| supervised, rate 1 | 1.46 s | 5.8x |

## Day 3 — Virtual pids and process lifecycle ✅

- **One launcher socket for the whole run** (not one per guest as the plan
  said), inherited by every guest at fd 240. Only the baton holder talks,
  so frames from different processes never interleave, and spawned
  children need no new connection. Frames: `Spawn` (path → rewritten
  path), `Spawned` (child index, real pid → ack once the launcher watches
  it), `Report` (replaces the per-guest report pipe).
- The launcher loop is a kqueue over the socket and `NOTE_EXIT |
  NOTE_EXITSTATUS` for every process of the run. It cannot reap guests'
  children, so their status comes from the event. Socket frames are
  drained before an exit is handled, so a dying guest's report is not lost.
- **The parent registers a spawned child itself** in the shared state (it
  holds the baton), so process and thread ids follow spawn order. The
  child starts parked and runnable; no launcher round trip for `Register`.
- `posix_spawn`, `posix_spawnp` (own `PATH` search), `fork`, `execve`.
  Children get our environment variables re-injected whatever `envp` the
  guest passed, and ASLR off. `execve` keeps the process record and the
  baton; the new image adopts the one live thread record. A `fork` child
  parks like a spawned one (the plan had the parent park; either is
  deterministic and this shares the spawn path).
- `waitpid`, `wait4`, `wait`: block on the process's wait key until the
  launcher has seen a child die, then a real `wait4` on the zombie.
  `WNOHANG` works. Process groups are not modelled (pid 0 or negative
  means any child).
- Virtual pids are `1000 + index`; the launcher is pid 1 to guests.
  `kill` with SIGTERM/SIGKILL retires the target's threads under the lock
  before sending, so the baton cannot go to a dead thread; other signals
  to other guests are logged and dropped.
- On-demand rewriting lives in `src/cache.rs` (moved from the
  CLI). An already rewritten file passes through, which is what a guest
  re-spawning itself via `_NSGetExecutablePath` names. Cache writes go
  through a rename.
- A manifest run's exit status reflects the manifest's own processes only.
- Test: `spawn_tree.c` (two `posix_spawn`, one `fork`+`execve`, one plain
  `fork`, reaped by pid and with `wait`). Pids 1000-1004, line order
  differs across seeds, 100 identical runs at seed 3.

## Day 4 — Readiness waits for pipes ✅

- `supervisor/src/io.rs`: `read`, `readv`, `write`, `writev`,
  `close` and their `$NOCANCEL` forms (stdio calls those, not the public
  names). Pipes and kernel sockets in blocking mode only; regular files,
  ttys and descriptors the guest made non-blocking pass through.
- Reads `poll` with a zero timeout first. Writes set `O_NONBLOCK` around
  each attempt and loop until the whole buffer is out, parking when the
  pipe is full. The flag flip is invisible to other guests because nobody
  else runs in between.
- All I/O waiters share one key and are woken (across processes) after any
  successful read or write on a pipe or socket, any close, and any process
  death. Reads wake too: draining a full pipe unblocks its writer.
- **External descriptors**: pipes and sockets on the launcher's own fds
  0-2 have their peer outside the run. The launcher exports their
  `dev:ino` and guests block on them for real; otherwise
  `echo x | rewrite run prog` would be reported as a deadlock.
- `io_waits` per process in the report.
- Test: `pipeline.c`, 200,000 lines through two pipes, every stage parks;
  correct on every seed, 100 identical runs at seed 2.
- Deferred to day 5 where it becomes testable: the (process, fd) → virtual
  socket map across `dup`/`fork`/`execve`. Kernel pipes need no
  bookkeeping of ours.

## Day 5 — Hosts and virtual stream sockets ✅

- `supervisor/src/netstate.rs` (part of the shared state, so the
  launcher compiles and unit-tests it): host table, 256 sockets with 64 KB
  receive rings, listen queues, names per host, ephemeral ports per host
  from 49152, per-process descriptor counts, and `deliver()` as the one
  place bytes move. The state file is now about 17 MB, sparse.
- `supervisor/src/net.rs`: the socket API. `read`/`write`/`readv`/
  `writev`/`close` in `io.rs` route to it first.
- **Descriptor identity instead of an fd table.** A virtual socket's
  descriptor is a real, never connected `AF_UNIX` socket. Its `st_ino` is
  unique and follows `dup`, `fork`, `execve`, close-on-exec and spawn file
  actions (which are opaque to us), so any process identifies a descriptor
  with one `fstat`. `O_NONBLOCK` lives on the placeholder too. What is
  tracked is only how many descriptors each process holds, so the last
  close (or a process death, swept by the launcher) sends FIN.
- A new process (spawn, fork, or a new image after `execve`) counts the
  virtual sockets it holds before it goes live, and `posix_spawn`/`fork`
  in the parent wait in real time for that. Otherwise the parent closing
  its copy right after the spawn could make the socket look unused.
- `connect` completes once queued on the listener, as TCP does (the plan
  had it park until `accept`); data sent before `accept` waits in the far
  end's ring.
- Errors per the plan: known host without listener `ECONNREFUSED`, unowned
  subnet address `EHOSTUNREACH`, another host's address in `bind`
  `EADDRNOTAVAIL`. An address outside `10.0.0.0/24` swaps the placeholder
  for a kernel socket at the same descriptor, is logged once and counted
  as `run.net_passthrough`.
- `fcntl` is interposed through an assembly shim: variadic arguments are
  on the stack on arm64 Darwin and stable Rust cannot declare that.
- Manifest hosts become `10.0.0.1` upward in the shared host table and in
  `REWRITE_HOSTS`. Single-program runs are on host `h0`.
- Report: `run.net_connections`, `run.net_bytes`, `run.net_passthrough`.
- Tests: seven unit tests of the state machine; `tcp_echo.c` over TCP
  between two hosts and over a UNIX-domain name on one host (names are per
  host, so that form cannot cross hosts as the plan's day table implied).
  Four connections, 820,120 bytes, 0 pass-through, 100 identical runs.

## Remaining

### Day 6 — options, datagrams, names, in-flight queue
- [ ] `setsockopt`/`getsockopt` answered from socket state (`SO_ERROR`,
      `SO_RCVTIMEO`/`SO_SNDTIMEO` need the virtual clock, day 7),
      `ioctl(FIONREAD)` (variadic: same shim as `fcntl`), non-blocking
      `connect` with `EINPROGRESS`
- [ ] Datagram sockets (`socket()` still passes `SOCK_DGRAM` to the kernel)
- [ ] `gethostname`, `getaddrinfo`, `getifaddrs`
- [ ] `deliver` through an in-flight queue with `deliver_at`
- [ ] `udp_ping.c`, `two_hosts.c`
- [ ] SIGPIPE on a write to a closed peer (today only `EPIPE`)

### Day 7 — shared virtual clock and timers
- [ ] Clock into the shared state (still per-process atomics in
      `determinism.rs`); deadline-ordered timed waits replace "release
      when idle"; sleeps become timed waits
- [ ] `poll`/`select` over mixed real and virtual descriptors (today they
      pass through and see the placeholder, which is never ready)

### Days 8-12
- [ ] `kevent` emulation; `poll_server.c`; `--net-latency`
- [ ] `counter_file.c`, `shared_map.c`; I/O calls as hook events; `flock`
- [ ] `net.rs`; switch cost in-process vs cross-process; throughput
      against kernel loopback; quantum defaults
- [ ] `docs/MULTIPROC_RESULTS.md`

### Decisions confirmed by Mark (2026-09-18)
- Rewritten binaries require the supervisor dylib; passive mode replaces
  standalone runs. No second stub flavor.
- No core pinning: the counter stays in the baton holder's L1 for the whole
  quantum, and a switch costs 8-14 us against about 0.1 us for the line to
  move. Spin-before-park is a day 10 experiment.

### Still open
- `LC_FUNCTION_STARTS` is dropped from tight binaries as the last resort.
- `~/dev/worklog` does not exist, so no work log entry was made.
- Branch `mlogan-multiproc` is not pushed.
