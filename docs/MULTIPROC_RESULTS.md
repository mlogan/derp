# Multi-Process Deterministic Runs: Results

Outcome of the experiment planned in `tracking/IMPLEMENTATION_PLAN_MULTIPROC.md`.
Code lives in the repository (rewriter, launcher, CLI) and `supervisor/`
(the injected dylib). Progress by day, and every place the implementation
departs from the plan, is in `tracking/TASKS_MULTIPROC.md`. How to use it and what a
guest must be: `README.md`.

All numbers are from one machine (Apple Silicon, macOS 26.2), release
builds, best of several runs.

## The three questions

### 1. Is the schedule of a multi-process run a function of the seed?

Yes, for every program tried, including processes that talk through pipes,
shared files, stream and datagram sockets, `poll`, `select` and `kevent`.
Each row is 100 consecutive runs with the same seed, comparing every
process's exit status and stdout and the run-wide schedule hash:

| Program | What it exercises | Processes | 100 runs |
|---|---|---|---|
| two `loops.c` | cross-process baton, 6,817 switches | 2 | identical |
| `spawn_tree.c` | `posix_spawn`, `fork`, `fork`+`execve`, `waitpid`, `wait`, virtual pids | 5 | identical |
| `pipeline.c` | 200,000 lines through two pipes, every stage blocks both ways | 4 | identical |
| `tcp_echo.c` TCP | two hosts, 4 connections, 820,120 bytes, backpressure | 2 | identical |
| `tcp_echo.c` `--unix` | UNIX-domain names on one host | 2 | identical |
| `udp_ping.c` | datagrams, lost ones resent, `getaddrinfo`, `FIONREAD` | 2 | identical |
| `two_hosts.c` | same port on two hosts, names, loopback, `getifaddrs` | 5 | identical |
| `timers.c` | sleeps, `cond_timedwait`, `SO_RCVTIMEO`, `poll`, `select` | 1 | identical |
| `poll_server.c` poll | 4 clients multiplexed, one dropped by idle timeout | 5 | identical |
| `poll_server.c` kevent | same, `EV_CLEAR` listener and `EV_ONESHOT` connections | 5 | identical |
| `net.rs` | Rust `std::net` and `std::process::Command`, memory hooks at 1/16 | 2 | identical |
| `tcp_echo.c`, `udp_ping.c`, both `poll_server.c` with `--net-latency 5ms` | payloads in flight | 2-5 | identical |

Hashes differ between seeds in every case, and outputs are correct on every
seed. No traffic between guests reaches the kernel: the report counts
connections, datagrams and bytes through the virtual network
(`run.net_*`), `run.net_passthrough` is 0 in every test, and a virtual
socket's descriptor is a placeholder that is never bound or connected.

With `--net-latency 5ms` the outputs stay the same, the schedule hash
changes, and the 100-run check still passes, so the seam a network
simulator will plug into (`Net::deliver`, the in-flight records, their
hookup to the virtual clock) works end to end.

### 2. Does the cross-process baton cost enough to change the quantum defaults?

No. A cross-process switch costs about twice an in-process one, but the
defaults are set by what finds races, not by switch cost.

Switch cost, two parties calling `sched_yield` 200,000 times each:

| Scheduler | Parties | Per switch |
|---|---|---|
| `origin/main`: in-process state, mach semaphores | two threads | 2.77 us |
| now: shared-memory state, shared ulock | two threads | 2.88 us |
| now | two processes | 5.41 us |

Moving the scheduler state into a shared file cost nothing in-process. The
cross-process figure is the address-space switch. On a cache-heavy workload
a switch costs more, because the incoming thread finds its working set
cold: two `loops 1` processes pay 8 to 14 us per switch.

That adds up at the default quantum of 1,000 to 10,000 hook events, which is
only about 11 us of guest time on `loops.c`:

| Quantum | Two `loops 1` processes | Switches | Over no switching |
|---|---|---|---|
| none | 0.157 s | 34 | |
| 1,000..10,000 (default) | 0.246 s | 6,817 | +57% |
| 3,000..30,000 | 0.200 s | 2,267 | +27% |
| 10,000..100,000 | 0.173 s | 633 | +10% |
| 100,000..1,000,000 | 0.159 s | 60 | +1% |

But larger quanta lose races in short programs. At 10,000..100,000 the
unlocked `counter_file.c` (four workers, 500 updates each) is exact on all
of seeds 1..20, where the default loses updates on 4 of them; its workers
only see one or two expiries each. `shared_map.c`, which loops 200,000
times, still loses updates on all 7 seeds that hook its store. So the
default stays, and `--quantum 10000..100000` is the setting for long
compute-bound runs where a 57% tax matters.

**Spinning before parking** was tried (`REWRITE_PARK_SPINS`, off by
default). On the yield ping-pong it cuts a switch from 2.9 us (threads) and
5.4 us (processes) to 0.37 us and lowers total CPU time, because the sleep
and wake system calls are skipped. On realistic workloads it does nothing
(`pipeline.c`, `tcp_echo.c`, `poll_server.c`: same times) or hurts (two
`loops` processes: 0.247 s to 0.288 s wall, CPU up by half), because a
quantum lasts far longer than the spin and the spinner competes for a core.
The schedule hash is the same either way. **Pinning to one core** was
considered and dropped: the counter is only touched by the baton holder, so
its cache line moves at most once per switch (about 0.1 us against
microseconds for the switch), and arm64 macOS has no thread affinity.

Hook overhead on `loops 3` with the relocated counter:

| Mode | Time | vs native |
|---|---|---|
| native | 0.24 s | 1.00x |
| rewritten, passive (no scheduling) | 0.41 s | 1.70x |
| supervised, branch hooks | 0.42 s | 1.72x |
| supervised, memory hooks 1/16 | 0.53 s | 2.20x |
| supervised, memory hooks 1 | 1.43 s | 5.9x |

The counter moved out of the image into the shared state at no cost per
hook. Measured the same afternoon, the old stub (local counter reached with
`adrp`) and the new one (shared counter reached with one `movz`) are both
1.61x, then both 1.66x. The plan's pointer-slot design would have added a
dependent load to every hook.

Virtual socket throughput against kernel loopback, 512 MB one way:

| Write size | Kernel loopback | Virtual sockets |
|---|---|---|
| 64 KB | 11.5 GB/s | 3.3 GB/s |
| 4 KB | 3.4 GB/s | 3.1 GB/s |

The virtual path is bounded by two baton switches per 64 KB ring fill
(16,385 switches for 512 MB), not by copying.

### 3. Does a seed reproduce a cross-process race?

Yes, the way it reproduces the two-thread race.

- `counter_file.c`, four processes adding to a counter in a file by read,
  add, write: with `--flock` it prints 2000 of 2000 on every seed. Without,
  4 of seeds 1..20 print less **with branch hooks only**, because interposed
  I/O calls are hook events and so put switch points between the read and
  the write. Seed 10 prints `total=500 expected=2000` with hash
  `3c2e6112e72aecf3` on 100 of 100 runs.
- `shared_map.c`, two processes incrementing a counter in a `MAP_SHARED`
  mapping: never wrong with branch hooks only. At `--mem-hook-rate 1/16`
  the store is hooked for seeds 30, 45, 48, 74, 92, 110 and 118 of 1..120,
  and every one of them loses updates. Seed 30 prints
  `total=301719 expected=400000` on 100 of 100 runs.

## What differs from the plan, and why

- **Rewritten binaries need the supervisor.** A default-linked C hello
  world has no `__DATA` segment and header room for one 72-byte segment
  command, so the image has nowhere to keep a writable word. The stubs
  address a fixed region at `0x78_0000_0000` that the dylib maps: a private
  page per process, then the shared state. "Standalone" became the dylib in
  a passive mode.
- **dyld refuses an image without `LC_UUID`.** When header room is short
  the rewriter drops an empty `LC_DATA_IN_CODE`, `LC_SOURCE_VERSION`, then
  `LC_FUNCTION_STARTS`. Only the smallest binaries reach the last step.
- **One launcher socket per run**, not per guest: only the baton holder
  talks, so frames never interleave, and spawned children need no new
  connection. The parent registers a spawned child in the shared state
  itself, which keeps process and thread ids in spawn order.
- **The launcher handles every process death.** After `NOTE_EXIT` it
  retires the process's threads and, if one held the baton, picks the next
  holder. `exit`, `_exit`, `abort` and crashes are one path, and a guest's
  stdio flush at exit still happens under the baton.
- **No (process, fd) table for virtual sockets.** A socket's descriptor is a
  real, never connected `AF_UNIX` socket whose `st_ino` names the virtual
  socket. `dup`, `fork`, `execve`, close-on-exec, `O_NONBLOCK` and opaque
  spawn file actions are then the kernel's bookkeeping. A new process counts
  what it inherited before it goes live, and its parent waits for that.
- **`connect` completes when queued on the listener**, as TCP does, instead
  of parking until `accept`.
- **In-flight payloads wait in a per-socket FIFO**, not one global queue.
  With one latency per link a socket's payloads are always in due order. A
  simulator that reorders replaces that storage, not `deliver`'s interface.
- **The clock advances on every yield**, not only on a switch. Otherwise a
  lone compute thread never lets a sleeper's deadline pass.
- **UNIX-domain names are per host**, so the `--unix` echo runs on one host.

## What it took to get there

- `os_sync_wait_on_address` is a libSystem wrapper; its `__ulock_wait2`
  lands in our own interposer. The dylib calls the raw ulock functions,
  which dyld does not rebind for the interposing image.
- `mmap` hints are unreliable. About 1 launch in 100 under load, something
  else lands first above the dyld shared region and pushes everything up;
  and the kernel ignores hints between roughly `0x5_0000_0000` and
  `0x70_0000_0000` (a hidden reservation, then the GPU carveout). A fixed
  `mach_vm_allocate` fails instead of replacing, so the region is reserved
  that way and mapped over with `MAP_FIXED`. The supervisor's allocator
  region got the same treatment and moved from `0x3_0000_0000` to
  `0x74_0000_0000`.
- stdio calls `read$NOCANCEL` and friends, not `read`; both are interposed.
- `fcntl`, `ioctl` and `open` are variadic, and on arm64 Darwin variadic
  arguments travel on the stack. Stable Rust cannot declare that, so a
  three-instruction assembly shim loads the first one into `x2`.
- libSystem's resolver connects to `/var/run/mDNSResponder` over `AF_UNIX`
  on the guest's own thread. A UNIX-domain connect with no virtual listener
  to a path that is a socket in the real filesystem leaves the virtual
  network.
- `dispatch_semaphore_wait` deadlines are computed by libdispatch from the
  real clock, so their distance is measured on the real clock and rounded
  to a millisecond.
- An edge-triggered `kevent` registration cannot be emulated by comparing
  readiness between two looks: the edge may come and go in between. Sockets
  keep arrival and window-open counters for it.
- `waitpid(-1)` in the launcher would steal children from other runs in the
  same process (parallel tests); exits are watched per pid with kqueue.
- A test helper that used two launcher invocations passed the extra options
  to only one of them, and for a day the latency test compared outputs of
  runs without latency. It is one invocation now (`rewrite run --capture`).

## Known limitations

- Guests are our binaries only: no `/bin/sh`, no platform binaries.
- Signals between guests: SIGTERM and SIGKILL only, to a parked process.
  SIGPIPE is raised on a write to a closed peer unless `SO_NOSIGPIPE` or
  `MSG_NOSIGNAL` says otherwise.
- Not modelled: `SCM_RIGHTS`, `AF_INET6`, raw sockets, multicast,
  out-of-band data, `sendfile`, process groups, `kevent64`,
  `EV_DISPATCH`, `EVFILT_TIMER` (it runs on the real clock), non-blocking
  `connect` returning `EINPROGRESS` (it completes at once), an unconnected
  datagram socket sending outside the virtual subnet. Socket options set
  before a connection leaves the virtual network are not replayed.
- Capacity is fixed: 1,024 threads, 256 processes, 256 sockets, 32 hosts,
  64 KB per socket buffer, 60 KB per datagram. The state file is about
  34 MB, sparse.
- Files are not virtualized beyond the scratch directory; the first path
  opened outside it is logged.
- Hook overhead is unchanged from the first experiment (1.7x branch-only
  against a 1.3x target); the deferred stub work is still deferred.

## Verdict

Multi-process runs are deterministic from a seed with pipes, files, locks
and a full enough socket API that Rust's `std::net` and a `kevent` server
run unmodified, and cross-process races reproduce. The network simulator has
its seam: one `deliver` function that already carries both hosts and the
virtual clock, in-flight records the clock drains, and an idle run that
jumps to the next arrival. What it costs is a cross-process switch at twice
the in-process price, which the quantum setting trades against race
coverage.
