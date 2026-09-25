# DERP: Deterministic Execution and Replay Platform

Derp is a tool for running one or more processes deterministically, without containerization, hypervisors, or emulation.
It works on whatever binaries your build system produces, or whatever you installed from homebrew.
Thread scheduling, syscalls, hardware counters, heap layout, network traffic, and more are all deterministic.

The main advantage of Derp over other similar tools is that you can use it locally on your Mac, and benefit from fast iteration speed.

Derp **is not a secure sandbox! Do not use it to run anything that you would not run directly**. It has some limited isolation features for convenience only.

Currently Derp only supports arm64 Mach-O executables on macOS.

The approach it uses is binary rewriting.
This is easy on arm64 because instructions are fixed-size.
Branches, syscalls, loads/stores, hardware randomness, etc are all replaced with a unconditional branch to a small generated stub.
This stub decrements a quantum counter, enters the scheduler if it reaches 0, and performs the task(s) of the original instruction, before jumping back to the original code.
All threads (in all processes) contend on a single lock (Claude calls it a "baton"), and the scheduler deterministically decides which thread will acquire the lock next.

Execution is pseudo-randomly deterministic according to the seed supplied when starting the process(es).
This allows us to do *Deterministic Simulation Testing* by repeating execution many times with different seeds.
When a bug is found, it can be reproduced by re-running with the same seed.

Derp can automatically search for the point in (virtual) time at which the bug occurred, in cases where the incorrect execution precedes its detection by a substantial amount of time.

## Capabilities and Limitations

Derp can be used to search for and reproduce race conditions and other timing-related bugs, errors in distributed systems, crashes, bugs in crash/restart recovery, and any other sort of application logic bug.

It cannot reproduce data races or memory ordering bugs: All threads are serialized, so neither can ever occur while running in Derp.

It uses a network simulator (currently very bare bones) for deterministic delivery of TCP and UDP traffic between processes.
It can also inject process faults (i.e. killing processes randomly) to test crash recovery and fault tolerance.

Several large systems including Postgres, SQLite, RocksDB, and Redis have been verified to run deterministically under Derp. See the examples directory.

Derp generally has a slow-down factor of 2x or less for sequential code.
Multi-threaded or multi-process systems are fully serialized, lose all parallelism, and slow down accordingly.

Processes observe a virtual clock, which is necessary for determinism. The virtual clock is not very well tuned to give realistic CPU timings. Any system that measures its own performance may believe it is running either ludicrously fast or slow.
However, timers should be predictable: A process that sleeps for 100ms should observe that roughly 100ms of time has elapsed.

Derp can use debug or optimized binaries equally well.  Debuggers will probably mostly work on a binary that Derp has rewritten, but I haven't tested this.

## Development

Derp was written entirely by Claude Fable and Opus 5.5. The idea behind it was mine. (I don't claim to have invented the binary rewriting technique I used, I just mean that I directed Claude on the high level design).

I built it mainly because I have always been frustrated by how few tools of this sort offer macOS support, and since I do most of my work on macOS I don't get to take advantage of them.
Also, I'm obsessed with Deterministic Simulation Testing, and wanted to explore a new approach.

Behind the high-level idea lies a mountain of small hacks. It would have taken years to write this manually, mainly because of the amount of debugging required.

The approach was to get "hello world" working, and then throw an escalating series of bigger challenges at Claude.
Each time, the goal was the same: No matter how many times the program is run, it must execute identically (and produce plausible output - crashing immediately would always produce identical output, but Claude isn't that dumb.)
Since this is a mechanically verifiable goal, Claude is able to churn away mostly autonomously, finding and patching one source of nondeterminism after another.
Several times I had to stop it from trying to solve things in a stupid way, but for the most part it simply found bugs, fixed them, and kept going.

## Known issues

Although I've tested this on large, non-trivial systems (like Postgres and SQLite) there are certainly still nondeterministic executions waiting to be found. Feel free to file an issue if you find one.

Only arm64 Mach-O on macOS is supported. Other operating systems or executable formats would probably be easy to support. Supporting x86_64 is more difficult because it doesn't have fixed-size instructions, but support would probably just be a matter of burning enough tokens.

# Everything below this point is AI written, reader discretion advised.

## Commands

| Command | What it does |
| --- | --- |
| `derp run [opts] prog args…` | Rewrite (cached), then launch under the supervisor. |
| `derp run [opts] --manifest FILE` | Several processes on virtual hosts under one scheduler (see the run file, below). |
| `derp repeat [opts] prog args…` | Run `--runs` times; exit status, stdout and schedule hash must agree. Also takes `--manifest`. |
| `derp bench [opts] prog args…` | Time native against rewritten, without scheduling. |
| `derp bisect [opts] --manifest FILE` | When was the failing seed's failure decided? See "When did a failing run go wrong?". |
| `derp suspects [opts] --manifest FILE` | Which loads and stores does the failing seed need? See "Which lines does a failing run need?". |
| `derp scan [opts] prog` | Print what the rewriter would hook. |
| `derp rewrite [opts] in out` | Rewrite and sign, without running. |
| `derp copy in out` | Round-trip a binary through the writer and re-sign it. |
| `derp cargo cargo-args…` | Cargo, with `derp cc` as the linker, for programs too big for their sites to reach the stubs (see "Big programs"). |
| `derp cc linker-args…` | The linker driver `derp cargo` installs. |
| `derp rooms prog` | The rooms `derp cc` would give a program, and why. |

## Options

Options come before the program. Those marked "run file" can also be set
there; the command line wins.

| Option | Default | Run file | What it does |
| --- | --- | --- | --- |
| `--seed S` | `0` | `seed:` | The run's seed. Everything the run decides is drawn from it. |
| `--quantum LO..HI` | `1000..10000` | `quantum:` | Hook events per scheduling quantum, drawn from this range. The default finds races in short programs and costs about 57% on two compute-bound processes; `10000..100000` costs about 10% and misses races in short programs. |
| `--mem-hook-rate R` | `0` | `mem-hook-rate:` | `0`, `1` or a fraction like `1/16`: hooks a sparse, seeded set of memory accesses. Races on plain memory need it; races through files and sockets do not. |
| `--heap-size N` | `32G` | `heap-size:` | Address space of each guest's heap, such as `512M` or `4T`. Only touched pages cost memory. |
| `--manifest FILE` | | | The run file, in YAML (below). Hosts get `10.0.0.1` upward in the order listed and are reachable by name. |
| `--net-latency T` | `0` | `net-latency:` | Virtual-time delay of traffic between different hosts, such as `5ms`, `250us` or `1s`. |
| `--switch-cost T` | `10us` | `switch-cost:` | How far each baton hand-off moves the virtual clock, quantum expiries included; a clock read moves it 1µs. Threads run one at a time on one clock, so the cost is kept low: at 1ms, the old default, a run of several threaded servers with hundreds of threads between them got so little done per virtual second that their own timeouts fired. The price is that code timing its own work finds it very fast. Only a run-file run can change it. |
| `--stop-after T` | | `stop-after:` | End the run at this virtual time: whatever still runs is killed at that point of the schedule, reported as `stopped`, and does not fail the run. For servers that never exit by themselves, and for comparing what a program did in a fixed span of virtual time. Needs the supervisor. |
| `--wall-limit T` | | `wall-limit:` | The same at a real time. Works for `--native` runs too, which have no virtual clock: every process reports `cpu_user_ns` and `cpu_system_ns`, and `run.cpu_*` sum them, so a program's cost natively and under the supervisor can be compared over the same span. |
| `--scratch DIR` | under the system temp dir | | Where a run file's host directories are made, fresh. An existing directory is only cleared if an earlier run made it. |
| `--capture` | off | | Run-file runs: each guest's stdout goes to `stdout.<index>` in the scratch directory instead of ours. |
| `--capture-stderr` | off | | And its stderr to `stderr.<index>`, the supervisor's messages about that guest included; without it they come out on ours. |
| `--runs N` | `100` (`repeat`), `20` (`bisect`) | | Repetitions for `repeat`; futures per probe for `bisect`. |
| `--jobs J` | `4` | | `bisect`, `suspects`: runs at a time. |
| `--resolution T` | `2ms` | | `bisect`: stop at an interval this short. |
| `--reseed-at T --reseed N` | | | From virtual time `T` on, every random stream (schedule, faults, heap layout, entropy) starts over from `N`. This is what `bisect` does at each probe. `--reseed` needs `--reseed-at`. |
| `--no-supervisor` | | | No scheduling: the dylib only provides the stubs' counter. |
| `--native` | | | Run the original binary, without the rewriter or the dylib. |
| `--aslr` | off | | Leave ASLR on. |

The report goes to stderr: `run.*` totals, then `p<index>.*` per process.

Two environment variables help with debugging:

| Variable | What it does |
| --- | --- |
| `REWRITE_TRACE=file` | Appends one line per baton switch (from, to, hook events issued, site, virtual clock). Diff two of them to find where two runs part ways; the schedule hash covers the same values. |
| `REWRITE_PARK_SPINS=N` | Makes a parking thread spin first. It only helps when the baton bounces back within microseconds; off by default. |

## The run file

The run file allows you to configure a "cluster" that is split among several "hosts".
For instance, you can start up a server as well as one or more clients that send load to it.
Host isolation is extremely primitive, and does little besides telling hosts on different processes that they have different IPs.


```yaml
seed: 7                  # optional; the command line overrides these
quantum: 1000..10000
heap-size: 32G           # address space of each guest's heap (the default)
mem-hook-rate: 1/16
net-latency: 5ms
switch-cost: 10us        # virtual time a baton hand-off costs (the default)
stop-after: 30s          # the run is over at this virtual time (default: never)
wall-limit: 60s          # or at this real time, native runs included
outside-network: refuse  # or allow: connections and name lookups beyond the
                         # virtual network reach the real one (input, unrepeatable)
env: { LOG_LEVEL: debug }   # for every process (guests start from a fixed environment)
pass-env: [SSL_CERT_FILE]   # inherited from yours on purpose; nothing else is
allow: [/opt/site-content]  # extra paths every host may touch
hosts:                   # in order: 10.0.0.1, 10.0.0.2, ...
  - name: alpha
    files: [site]        # copied into the host's fresh directory
    processes:
      - [server, --port, 8080]          # argv verbatim
      - client alpha 8080               # or a line, split on whitespace
      - argv: [worker, "two words"]     # or a map, with an environment
        env: { MODE: fast }
      - argv: [httpd, --port, 80]       # a server that never exits: killed
        daemon: true                    # when every other process is done
```

Guests of a run file do not inherit your shell's environment. They start
from a fixed one (`PATH=/usr/bin:/bin:/usr/sbin:/sbin`, `LANG=C`, `LC_ALL=C`,
`TZ=UTC`, `USER=guest`, `LOGNAME=guest`, and `HOME`, `PWD`, `TMPDIR` in the
host's directory). Add to it in the run file: a top-level `env:` map for
every process, `env:` on a process, or `pass-env: [NAME, ...]` to inherit
named variables on purpose. Otherwise a proxy setting or a locale would be
an input nobody wrote down, and the environment's length even decides where
a guest's stack starts. Single-program `derp run prog` still inherits.

A process written as a map can be crashed on purpose and restarted:

```yaml
      - argv: [server, --port, 7000]
        daemon: true
        restart: on-failure      # never (default) | on-failure | always
        restart-delay: 50ms..150ms   # how long it stays down (default 100ms)
        max-restarts: 10         # default: no limit
        crash:
          every: 100ms..300ms    # into each life
          times: 3               # default: no limit
```

All times are virtual and drawn from the seed, so a seed crashes the same
process at the same point of the same schedule every time. A crashed
process is down for its restart delay (connections to it are refused),
then comes back as a new process on the same host, with a new pid and the
same host directory; what it needs to remember it has to have written
there. Its captured stdout continues in the same file. `on-failure` means a
signal or a non-zero exit. The run's exit status is that of each entry's
last life. The report counts `run.crashes_injected` and `run.restarts`,
and `p<i>.entry` says which run-file entry process `i` was a life of;
`p<i>.program` and `p<i>.image` say what it ran, children a guest started
included.

`restart-delay` may not be zero, and it and `max-restarts` need a restart
policy. Only run-file entries are restarted, not a guest's own children. A
guest that `kill`s a restartable process causes a restart like any other
death. These keys need the supervisor (no `--native`, `--no-supervisor`),
and a run holds 1024 processes, each life counting as one.

A process written as a map may also say `daemon: true`: a server that never
exits. When every other process of the run file has exited, the launcher
kills what is left and the run is over.

Guests do not outlive the launcher. One whose launcher is gone (killed,
crashed) exits with status 70 within about a second: nobody is left to
hand it the baton, end the run or release a lock the launcher held.

Processes start in file order. `argv[0]` names the program, relative to the
run file, and reaches the guest exactly as written. Numbers and booleans in
an argument list are taken as their text; quote one whose spelling matters
(`"007"`). Unknown keys are errors.

## A directory per host

Every run the launcher makes a fresh directory for each host, under the
scratch directory and named after the host. The run file never names it. A
host's `files:` (paths relative to the run file) are copied in first:

```yaml
hosts:
  - name: web
    files: [site]        # becomes <host directory>/site/...
    processes:
      - [server, --root, site]
```

Processes start in their host's directory, with `PWD`, `HOME` and `TMPDIR`
inside it, and the supervisor holds their path names to it. A path must be
under the host's directory, under a system location (`/usr`, `/etc`, `/dev`,
`/opt/homebrew`, ...), or under one of the run file's top-level `allow:`
entries; anything else fails with `EACCES` and is logged with the host, the
call and the path. Asking about the directories above the host's (`stat`,
`access`) is allowed; opening them is not. A relative `allow:` entry is
next to the run file, like a program, and is followed through symlinks:
`allow: [src]`, with `src` a link to a checkout whose path a program has
built in, lets every host read that checkout wherever it is.

This guards against configurations that would let hosts share files by
accident, such as an absolute path into another host's directory or a
`../other/data`. It is not a sandbox: paths are checked as text, so a
symlink gets out. Program images are not checked.

Results: `docs/REWRITE_RESULTS.md` (single process) and
`docs/MULTIPROC_RESULTS.md` (several processes, virtual network).

## What a guest must be

- An arm64 `MH_EXECUTE` with `LC_FUNCTION_STARTS`, without the hardened
  runtime or library validation (so `DYLD_INSERT_LIBRARIES` is honoured).
- Default link flags are fine. No `-headerpad` is needed.
- Every process in a run must be one of ours: children a guest spawns are
  rewritten on demand. Apple's own binaries (`/bin/sh`, `/usr/bin/curl`,
  `/usr/bin/python3`) ignore `DYLD_INSERT_LIBRARIES` and cannot be guests.
  Homebrew's can: its `curl` and `python3.13 -m http.server` run
  repeatably. Rewritten copies of installed programs go to
  `$TMPDIR/rewrite-cache/`, not next to the program.
- Threads the guest makes with `pthread_create` are scheduled. GCD worker
  threads are not, so **a guest may not submit work to Grand Central
  Dispatch**: `dispatch_async`, `dispatch_after`, `dispatch_apply`, dispatch
  groups, sources and I/O end the run with exit status 69 and a message
  naming the call. `dispatch_sync` and dispatch semaphores are fine. System
  libraries may use GCD internally; what they do there is input.
- Blocking must go through something the scheduler sees, or the thread
  sleeps in the kernel holding the baton. Seen: pthread mutexes, condition
  variables (also `pthread_cond_timedwait_relative_np`, which is Rust's
  `Condvar::wait_timeout`) and rwlocks, `os_unfair_lock` and the ulock
  calls, dispatch semaphores, sleeps, `poll`/`select`/`kevent`, socket and
  pipe I/O, `waitpid`. An async runtime works: tokio's multi-thread
  runtime, reactor, blocking pool, timers and `tokio::sync` are tested
  (`tests/programs/kv`), as are `tokio::fs`, `process` and `signal`. A
  deadlock report lists what each thread waits for.
- A wait (`poll`, `select`, `kevent`) may not depend on a descriptor whose
  other end is outside the run: nothing could repeat it. Such a wait is
  logged, and only the guests' side ever ends it.
- In a run-file run, connecting to an address outside the virtual network
  fails with `ENETUNREACH` and looking up a name the run does not know
  fails with `EAI_NONAME`, unless the run file says `outside-network:
  allow`; then they reach the real network and what comes back is input.
  A lone `derp run prog` allows them.
- A wait a system library makes for itself (libdispatch for a block on a
  GCD worker, libxpc for a reply: what the Security framework does to load
  certificates, what the resolver does) is made in the kernel with the
  baton: nothing else in the run moves until it is over, so however long
  it takes, the run is the same. The report counts them as
  `system_wait`.
- A handler for any other signal runs when the kernel delivers it, at a
  moment of real time, on whichever thread it lands, usually one that is
  parked. What it does is input to the run. It may call anything; a wake
  it causes while its thread is inside the scheduler is made when that
  thread leaves it.
- `SIGCHLD` handlers run at a point of the schedule (the parent's next
  thread to run after the death), not when the kernel sends the signal. A
  signal a guest sends itself is delivered to the calling thread.
- A guest's heap is 32 GB of address space, of which only touched pages
  cost memory. `heap-size: 128G` in the run file or `--heap-size` changes
  it (64 MB to 4 TB). A seeded layout scatters blocks, so leave it roomy:
  a quarter holds the small size classes and the rest the large blocks.
- Go programs are supported in part. Build with the external linker
  (`go build -ldflags=-linkmode=external`: Go's own emits no
  `LC_FUNCTION_STARTS`) and set `GODEBUG=netdns=cgo` in the run file so
  that Go resolves the run's host names through the system resolver. A
  Go server and client (`examples/go`) run and their output repeats; the
  schedule hash of that pair takes one of two values, one quantum ending
  one hook apart in the runtime's stack copying. A Go program alone
  repeats fully.
- A guest with an allocator of its own (jemalloc, sui-node's default)
  keeps its heap out of the seeded one, so its layout is not the seed's
  to vary, and a bug that depends on pointer order shows on fewer seeds.
  It runs repeatably all the same: its memory comes from `mmap`, which
  the run places, and its per-thread cleanup runs before the next thread
  does (below). Build with the system allocator to get the seeded layout
  (`--no-default-features` for sui-node).
- A thread's exit is complete, for the schedule, when the kernel says the
  thread is gone. The supervisor hands the baton on from the exiting
  thread's key destructor, and destructors of keys the guest made later
  (jemalloc's thread cache, which re-arms itself for every round) still
  run after that, off the baton; the next thread to run in that process
  waits, in real time, until the exiting thread's Mach port is dead. The
  report counts `exit_waits` and the longest, `exit_wait_max_ns`.
- Heap addresses are a function of the seed, and differ between seeds:
  how two blocks compare, and whether `free` then `malloc` returns the
  same block, goes both ways across seeds. A bug that depends on pointer
  order shows on some seeds and replays on those.
- Guests see virtual pids from 100,000 up (above any real pid), in spawn
  order; the launcher is pid 1. A scheduled thread's `pthread_threadid_np`
  is its index in the run plus a billion (kernel thread ids differ from
  run to run; RocksDB mixes one into its DB session ids).
- System V semaphores (`semop`, what Postgres's lightweight locks sleep
  on) are waited for by the scheduler, and a keyed segment or set a
  guest creates (`shmget`, `semget`) is private to the run and removed
  when it ends, as is a POSIX shared memory object (`shm_open`, given a
  per-run name): keys and names are a namespace of the whole machine,
  and what one run leaves there would change the next run's tries. A
  segment attaches (`shmat`) in the reserved region. `getrusage` reports
  the virtual clock as CPU time.
- A process's interval timer (`setitimer(ITIMER_REAL)`, `alarm`) is a
  virtual deadline: its SIGALRM is pending from that moment of the
  schedule and wakes a blocked thread of the process, instead of landing
  on a parked thread at a real moment (Postgres times statements so).
- A signal one guest sends another (`kill` with anything but SIGTERM and
  SIGKILL, which end the target) is delivered when the target next takes
  the baton up, on that thread, so its handler runs at a point of the
  schedule; `kill(pid, 0)` says whether a guest lives. A kqueue watch on
  a guest's pid (`EVFILT_PROC`) or on a signal (`EVFILT_SIGNAL`) is the
  run's too: Postgres's children watch the postmaster and its latches are
  SIGURG.
- A read of the CPU's counter register (`mrs xN, cntvct_el0`, what Redis
  and others take their monotonic clock from, past every library) is
  rewritten into a read of the virtual clock, in the counter's ticks. The
  report of `derp scan` and the "sites hooked" line count them.
- Where a scheduled thread's mappings land is the run's: thread stacks,
  `pthread_t` blocks and `mmap`s without an address go to a reserved
  region (64 GB at `0x7c_0000_0000`) in schedule order, so the kernel's
  placement of what it maps meanwhile (GCD workers' stacks) cannot move
  them. `pthread_self` is a stack address, and RocksDB seeds its skip-list
  heights from it. An `mmap` with an address hint is placed like one
  without: the kernel frees an exited thread's stack itself, in real
  time, and would grant a hint into the hole once it had (jemalloc hints
  at the end of its last extent). The report counts `mappings_placed`
  and `mappings_hinted`. An exited thread's stack is not reused; the
  region has room for about 30,000 threads of 2 MB.

## Sources of nondeterminism found and fixed

Every row names an input the run took from outside itself, or a bug in
the supervisor, and what was done. Keep these tables current: a change
that closes a source of nondeterminism adds a row here.

### Scheduling

| Problem | Fix |
| --- | --- |
| Thread interleaving is decided by the kernel | One thread of the run holds a baton and runs; the others are parked. The baton changes hands at quantum expiries, counted in hooked branches and calls, and at every interposed blocking call. The quantum length is drawn from the seed. |
| Blocking calls sleep in the kernel while the thread holds the baton | Mutexes, condition variables (also `pthread_cond_timedwait_relative_np`), rwlocks, `os_unfair_lock` and the ulock calls, dispatch semaphores, sleeps, `poll`, `select`, `kevent`, pipe and socket I/O, `waitpid`, `semop` and `pthread_join` are interposed and become scheduler waits. |
| Several processes schedule independently | The scheduler's state is in shared memory and the baton passes between processes; `fork`, `execve`, `posix_spawn`, exit and `waitpid` are points of the schedule. |
| `POSIX_SPAWN_SETEXEC` was treated as a spawn: the old process record kept the baton | It is treated as an exec. |
| GCD worker threads are created by the kernel and run outside the scheduler | A guest that submits work to Grand Central Dispatch ends the run with exit 69. System libraries use GCD internally: their wakes go to the scheduler and the kernel, an unfair-lock wait whose owner is such a thread is a real wait, and an idle scheduler looks again in real time for up to 30 s before declaring a deadlock. |
| A wait a system library makes for itself (an XPC reply, a block on a GCD worker) yielded the baton and let real time in | Such a wait is made in the kernel with the baton held, so nothing else moves meanwhile. |
| libdispatch's ulock waits pass `ULF_NO_ERRNO` and got positive errors | The interposer answers those callers with negative errno values. |
| The thread-exit hook's join wake counted as a wake from outside the schedule (libpthread clears the key before the hook runs) | The hook sets the identity back for the wake. |
| Key destructors of the guest ran after the exiting thread had handed the baton on | The supervisor's key destructor re-arms itself for every destructor round but the last, so the guest's run with the baton. |
| Destructors of keys younger than the supervisor's (jemalloc's thread cache) still ran in the last round, off the baton | The exiting thread leaves its Mach port behind and the next baton holder in that process waits until the port is dead. |
| An exiting thread's last hooks raced the next holder's quantum | The wait for the port happens before the holder touches the hook counter. |
| A signal handler on a thread that is inside the scheduler lock deadlocked on the lock | The lock word names the holding thread; a handler on the holder makes no wake itself and leaves it for the holder to make on leaving the lock. |
| A process that died holding the scheduler lock hung the run | The lock is taken over from a dead owner, whose death is checked with `proc_pidinfo` (a zombie that is nobody's child still answers `kill`). |
| A new image after `execve` spun on a lock word naming its own pid | The word is reset before the new image's first lock. |
| Threads outside the schedule ran hooked code and ate the holder's quantum | The report counts expiries taken without the baton (`outside_expiries`, `stray_expiries`) and the trace names them. |
| Real-time collisions on libmalloc's lock inside the supervisor showed in the guest's schedule | The supervisor allocates from its own heap with its own lock, and only the baton holder's unfair-lock waits count. |
| A daemon was killed at the end of a run, unheard | When every other process is done the launcher arms a stop at the virtual time reached; every thread of a stopped process is made runnable and the first to take the baton up writes the report and ends the process. |
| The run started as soon as every guest had attached, so a guest's remaining start-up ran in real time while the first guest already ran guest code. Two Go processes fell into one of two schedules from the first quantum | The launcher hands the baton out only once every guest's main thread is parked. |
| A thread-directed signal (`pthread_kill`) from one scheduled thread to another landed on a parked thread at a real moment. Go preempts goroutines and stops the world with SIGURG this way | The signal is pending against the target thread and raised when it next takes the baton up. |
| A process that leaves through `_exit` skipped the exit hooks and never reported. Go does | `_exit` writes the report first. |
| The supervisor timed its own wait with `Instant::now()`. libSystem's clock call is routed to the interposer by dyld, so every spin moved the virtual clock | The supervisor reads real time only with `mach_absolute_time` directly. |

### Memory

| Problem | Fix |
| --- | --- |
| `malloc` addresses vary between runs | The malloc family is interposed and served from a heap in a fixed region. |
| The heap layout was the same for every seed, so pointer-order bugs never showed | Where blocks land and whether a freed block is reused at once are drawn from the seed and the process index. |
| A fixed 4 GB heap ran out under a scattered layout | The heap is 32 GB of address space by default and the run file sets it. |
| `mmap` hints for the supervisor's regions were not honoured. The kernel ignores hints in some ranges and low addresses vary between launches | The regions are reserved with `mach_vm_allocate` at fixed addresses and mapped over with `MAP_FIXED`. |
| Threads outside the schedule allocated from the deterministic heap | Their allocations go to libmalloc, and a block they free is counted as leaked rather than reused. |
| The kernel placed thread stacks first-fit around GCD workers' stacks, and `pthread_self` is a stack address that RocksDB hashes | Scheduled threads' `mach_vm_map`, `mach_vm_allocate` and `mmap` requests without an address are placed in a reserved region in schedule order. |
| An `mmap` with an address hint was granted or refused by the kernel depending on whether an exited thread's stack had been freed yet | Hinted requests are placed like the others. |
| `shmat` attached a segment where the kernel chose | It attaches in the reserved region. |
| A guest's own allocator (jemalloc) keeps its heap out of the seeded one | Its memory still comes from `mmap`, which the run places, so it runs repeatably, with the compact layout. |
| The yield and counter entries pushed several hundred bytes of registers onto whatever stack the hooked code ran on. A goroutine's stack has a guard of under a kilobyte, and the pushes landed on the heap object below it (Go's collector found a corrupted heap) | Each scheduled thread has a supervisor stack for those entries, which leave at most 48 bytes on the guest's stack. |
| The environment's length decides where a guest's stack starts | The supervisor's own variables are fixed-width, run-file guests start from a fixed environment (`pass-env:` names what is inherited), and the trace and mask paths reach guests through the shared state rather than their environment. |

### Time

| Problem | Fix |
| --- | --- |
| Clock reads return real time | `clock_gettime`, `gettimeofday`, `time`, `mach_absolute_time`, `mach_continuous_time` and `clock_gettime_nsec_np` return a virtual clock that advances 1 µs per read and 10 µs per baton switch, and jumps to the next deadline when nothing can run. |
| Threads outside the schedule advanced the virtual clock when they read it | They see the clock but do not move it. |
| The Mach clock services returned real time. RocksDB's `NowNanos` on macOS asks `host_get_clock_service` for the calendar clock and reads it with `clock_get_time`, a call to the kernel. It mixes that time into the entropy for its DB ids and session ids, which key its block cache: a Sui fullnode walked its cache in another order from run to run, and one quantum in two million ended elsewhere | The two calls are interposed; the calendar clock is the run's real time and the system clock its monotonic time. |
| The system allocator's own time reads moved the clock. libsystem_malloc reads `mach_absolute_time` in `free` as often as its heap's state says, and GCD workers shape that state in real time. CoreFoundation freeing on a scheduled thread (the Security framework loading certificates in sui-node) ticked the clock once more or less, and a Sui cluster parted within two virtual seconds on five runs in six | A read whose caller is in libsystem_malloc sees the clock and does not move it. |
| `gettimeofday` left its time-zone argument unfilled. Redis takes its zone from it and logged dates in 1970 | It is filled with UTC. |
| Reading `CNTVCT_EL0` returns the CPU's real-time counter | The `mrs` instruction is rewritten to jump to a stub that returns the virtual clock in the counter's ticks. |
| `setitimer` and `alarm` armed real kernel timers whose SIGALRM landed on a parked thread at a real moment | The timer is a virtual deadline of the process; its SIGALRM becomes pending at that moment of the schedule and wakes a blocked thread of the process. |
| `getrusage` returns real CPU times | It returns the virtual clock as user time. |
| Timed waits across processes used each process's own clock | The run has one clock in the shared state, and timed waits are ordered by deadline on it. |

### Randomness

| Problem | Fix |
| --- | --- |
| `arc4random`, `getentropy` and `CCRandomGenerateBytes` return real entropy | They return bytes from a stream seeded by the run's seed and the process index. |
| Reads of `/dev/urandom` and `/dev/random` return real entropy (Redis seeds its hash tables there) | Descriptors open on those devices read from the same stream. |

### Signals

| Problem | Fix |
| --- | --- |
| A signal a guest sends itself went to whichever thread the kernel picked, at a real moment | `kill(getpid(), sig)` is delivered with `pthread_kill` to the calling thread, so the handler runs there with the baton. |
| `SIGCHLD` arrived from the kernel at a real moment | The guest's handler is kept by the supervisor, the kernel's delivery is dropped, and the handler runs on the parent's next thread to take the baton up after the death is recorded, with the child's pid and status in `siginfo`, `SA_RESETHAND` honoured, and a blocked signal left for a thread that takes it. |
| Signals between guests were dropped (Postgres's latches are SIGURG, its procsignals SIGUSR1) | A signal to another guest is recorded against the target and raised on the target's next thread to take the baton up; `kill(pid, 0)` answers whether the guest lives. |

### Processes

| Problem | Fix |
| --- | --- |
| Virtual pids collided with real ones and reached system APIs | Virtual pids start at 100,000, above any real pid; callers inside the dyld shared cache get the real pid from `getpid` and `getppid`. |
| Kernel thread ids differ from run to run (RocksDB mixes one into its session ids) | `pthread_threadid_np` returns the thread's index in the run plus a billion. |
| A guest killed by another guest was dead to the run only when the launcher noticed | The whole death happens under the scheduler lock at the moment of the `kill`; its peers see EOF from then. |
| Guests outlived a dead launcher | A guest whose launcher is gone exits within a second. |
| A guest's own children were invisible to the tools | Every process reports its program once it first holds the baton, and the launcher keeps the rewritten-to-original mapping. |

### Files and IPC

| Problem | Fix |
| --- | --- |
| Guests read and wrote wherever their paths pointed | Every host gets a fresh directory per run, `files:` copies inputs in (with their modes), and paths outside it and the system's directories are refused. |
| System V keys are a namespace of the whole machine, and Postgres tries keys in sequence until one is free | A keyed `shmget` or `semget` makes a private object, and the launcher removes the run's objects at the end. |
| POSIX shared memory names collided with a killed run's objects | `shm_open` names carry the launcher's pid and are unlinked at the end. |

### Network

| Problem | Fix |
| --- | --- |
| Sockets between guests went through the kernel | Guests live on virtual hosts with virtual stream and datagram sockets, names, `poll`, `select` and `kevent` over them, payloads delivered on the virtual clock with a fixed latency. |
| A guest could reach the real network, whose answers cannot be repeated | In run-file runs, connections outside the virtual network fail with `ENETUNREACH` and unknown names with `EAI_NONAME` unless the run file allows them; a connection that leaves is logged with its destination. |
| A lookup that needs no resolver (a null host for a port, `localhost`, a numeric address) went to libSystem's resolver, whose threads and sockets are outside the run. Go's cgo resolver asks it for every port | Such lookups are answered by the supervisor. |
| An IPv6 socket was the kernel's, and a dual-stack listener (Go's, Postgres's) lived outside the run while clients connected to the virtual address | In a run, `socket(AF_INET6)` fails with `EAFNOSUPPORT`, and programs fall back to IPv4. |
| `send` on a kernel socket pair woke nobody (tokio's signal self-pipe) | `send`, `sendto` and `sendmsg` wake the scheduler's waiters, and `recv` on such a socket waits in the scheduler. |
| `recv(MSG_DONTWAIT)` on a blocking socket pair parked | It does not. |

### kqueue

| Problem | Fix |
| --- | --- |
| A kqueue holding only a user event waited in the kernel with the baton (mio's waker) | A `kevent` wait is the scheduler's unless every registration belongs to the outside world; user events are modelled. |
| `EV_RECEIPT`, `EV_DISPATCH`, `EV_CLEAR`, `EV_ONESHOT` and `EV_ENABLE` re-firing were not modelled | They are, checked against the kernel's answers, with receipts in change order. |
| mio registers through one duplicate of the kqueue and waits on another | Duplicates share one registry entry, and closing a descriptor purges its entries without touching timer or process registrations of the same ident. |
| `EVFILT_PROC` on a virtual pid was rejected by the kernel (Postgres's children decided the postmaster had died) | Watches on a guest's pid and on signals (`EVFILT_SIGNAL`) are the run's, fed by the process table and the signal counts. |
| A wait mixing guest descriptors with ones from outside the run is not repeatable | It is logged once, and only the guests' side ends it. |

### Rewriting

| Problem | Fix |
| --- | --- |
| Sites more than 128 MB from the stub segment cannot reach it | One shared stub body with a four-to-six-word trampoline per site, far callees through x16 islands at call sites, and rooms planted in the text at link time by `derp cargo` for the sites still out of reach. |
| A store-conditional fails when an interrupt lands between it and its load, and the retry branch after it was hooked: the retry happened at the hardware's whim and counted a hook | The branch right after an exclusive store is part of the untouched span. |
| Constant tables in hand-written assembly decoded as branches (blst's SHA-256 constants) | Function-table entries without a symbol are left alone when most entries have one. |
| Tail calls through x16 clobbered a register hand-written assembly keeps live | Islands are used at `bl` sites only. |
| Rewritten images of installed programs were written next to the program | They go to a cache directory under `$TMPDIR`. |

## Big programs

Stubs are one shared body per program and a four-to-six-word trampoline
per hooked site, so a release build of sui (106 MB of code, 2.5 million
sites) has 45 MB of them. Every site must reach its trampoline, and the
trampoline its target, with one `b` (±128 MB). The stub segment goes
right after `__DATA`, so in a program whose code runs past about 120 MB
the earliest sites cannot reach it: they are left as they are and counted
(`unreachable` in `derp scan`, and in the "sites hooked" line; 2.5% of
sui's sites). Code that is not hooked runs without preemption until it
reaches a hook or a blocking call; the run stays deterministic. A callee
too far for a `b` is reached through x16, as a linker's branch island
would. Below `__TEXT` is not an option: the kernel wants `__PAGEZERO` to
cover the low 4 GB and reserves everything up to the first segment.

For the rest, room is made at link time: `derp cargo build …` runs
cargo with `derp cc` as the linker (through the
`CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER` variable; the program's own
`Cargo.toml` and `.cargo/config` are not touched, and nothing happens
unless you ask). `derp cc` links as `cc` would, and if the result has
sites out of the stubs' reach it links once more with a `.space` object
per 128 MB of text, placed in the middle of its stretch by an order file
listing the symbols before it (`<program>.rooms/`). The rewriter writes
the far sites' trampolines into those rooms, which are ordinary text.
`<program>.rooms/report.txt` says what it did: how many sites were out of
reach, how many rooms it made and how big, and how many sites still
cannot be reached (cargo shows a linker's messages only when the link
fails). `derp rooms <prog>` shows the plan for a program without
linking it. A `-O0` `sui-node` (180 MB of code, 4.8 million sites, 70% of
them out of reach) gets two rooms of 76 MB together and every site is
hooked; `derp run` then says how much went into rooms. `derp run` on a program that needed this and did not
get it says so. No arm64 instruction reaches further than a `b` in one
word, and stubs below `__TEXT` are impossible: the kernel wants
`__PAGEZERO` to cover the low 4 GB and reserves everything up to the
first segment.

A function-table entry without a symbol is a constant table in
hand-written assembly (blst keeps its SHA-256 round constants that way),
whose words would be hooked as instructions; such entries are left alone
when the file names at least half of its functions (`unnamed_entries`).
A room's order file leaves the megabyte around such an entry unlisted,
so that it and the code reaching it with an `adr` stay together.

## Rewritten binaries only run under the supervisor

The stubs do not address anything in the image. They reach the quantum
counter and the scheduler entry point in a fixed region at
`0x78_0000_0000` that the supervisor dylib maps at startup (one private
page per process, then the run's shared scheduler state). A default-linked
binary has header room for one new segment, which holds the stub code;
there is nowhere to put a writable word. Running `prog.rw4-…` directly
faults at the first hooked branch.

`--no-supervisor` and `bench` still inject the dylib, in a passive mode
that maps the region and schedules nothing. It adds under 1 ms of startup.

## When did a failing run go wrong?

```
derp bisect --seed 5 --manifest run.yaml
```

A race may corrupt something long before an assert notices. `bisect`
replays the failing seed and, at a virtual time `t`, gives the scheduler a
new random stream: the run is the failing run until `t` and some other
future after. Once the damage is done nearly every future fails; before,
only as many as ever did. It measures that with `--runs` futures per probe
(default 20, `--jobs` at a time), binary-searches `t` down to
`--resolution` (2 ms of virtual time), and prints the probes, the interval
in which the failure became certain, and the failing run's thread switches
inside it. `--reseed-at T --reseed N` on `derp run` replays one such
future. Every random stream starts over at `T`: thread choice and quanta,
injected faults, where heap blocks land, and the entropy guests read. So a
failure decided by pointer order or by a random value is found the same
way; its moment is the allocation or the draw.

## Which lines does a failing run need?

```
derp suspects --seed 1 --mem-hook-rate 1 --manifest run.yaml
```

With memory hooks on, a failing seed switched threads at some loads and
stores. `suspects` asks which of them the failure needs: it forbids
switches at all but a subset (every stub still counts, so nothing else
moves), and shrinks the subset by delta debugging until no site can be
dropped. It prints those sites with function, file and line from the
original binary's debug symbols (build with `-g`). If the failure needs no
switch at any load or store it asks the same of branches and calls; a
suspect of that kind is where the thread was switched out, and the shared
access is near its caller. `REWRITE_MASK` is the file of forbidden sites it
gives each run.

## Debugging a guest

lldb works on rewritten binaries. Text is patched in place and the UUID is
kept, so the original debug information still fits: `breakpoint set --file
F --line N` resolves to the same address as in the original, a breakpoint on
an instruction the rewriter replaced still hits, and variables, backtraces,
`next` and `finish` behave. You can launch a rewritten file under lldb with
`DYLD_INSERT_LIBRARIES` set to the supervisor dylib, or attach to a guest of
a real run by pid.

- A debugger finds a dSYM by the executable's file name, so the rewriter
  links `<rewritten file>.dSYM` to the input's bundle when it has one.
  Debug information kept in object files needs nothing.
- `step` into a hooked call stops inside its stub, which has no symbol, line
  or unwind information, and from there only steps instructions. Set a
  breakpoint on the callee and `continue` instead.
- Time is virtual: a guest that only sleeps is over at once in real time,
  possibly before you attach.
- A stopped guest holds the baton, so the run stays deterministic; what you
  change from the debugger is input.

## Small binaries lose `LC_FUNCTION_STARTS`

The rewriter adds one 72-byte segment command and must leave 16 bytes for
`codesign`. When the header is too tight it drops commands nothing reads at
run time, in this order: an empty `LC_DATA_IN_CODE`, `LC_SOURCE_VERSION`,
`LC_FUNCTION_STARTS`. `LC_UUID` stays, because dyld refuses an image
without one.

Only the smallest binaries get as far as the last step: a default-linked C
hello world does, a default-linked Rust program does not. For those:

- debuggers and profilers see no function boundaries in the rewritten file
  (the original is untouched);
- the table's bytes stay in `__LINKEDIT`, unreferenced;
- the rewritten file cannot itself be rewritten. The launcher never tries:
  a file that already has a `__STUB` segment is used as it is.

Linking the guest with `-Wl,-headerpad,0x1000` leaves room for everything
and avoids the drops. If even the drops are not enough, the rewriter fails
with an error that says so.
