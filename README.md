# rewrite

Rewrites arm64 Mach-O executables so that branches (and a sparse set of
memory accesses) pass through stubs, and runs them under a supervisor
dylib that owns the thread and process schedule. Given a seed, the
interleaving of every thread in every process of a run repeats exactly.

```
rewrite run    --seed S [--mem-hook-rate R] prog args…
rewrite run    --seed S --manifest FILE        # several processes on virtual hosts
rewrite repeat --seed S --runs N …             # status, stdout and schedule hash must agree
rewrite bench  prog args…                      # native vs rewritten, no scheduling
```

Options worth knowing:

- `--mem-hook-rate 1/16` hooks a sparse, seeded set of memory accesses;
  races on plain memory need it, races through files and sockets do not.
- `--quantum LO..HI` is hook events per scheduling quantum. The default
  `1000..10000` finds races in short programs and costs about 57% on two
  compute-bound processes; `10000..100000` costs about 10% and misses races
  in short programs.
- `--manifest FILE` is the run file, in YAML (below). Hosts get `10.0.0.1`
  upward in the order listed and are reachable by name. `--net-latency 5ms`
  delays traffic between different hosts in virtual time.
- `--stop-after 30s` ends the run at that virtual time: whatever still
  runs is killed at that point of the schedule, reported as `stopped`,
  and does not fail the run. For servers that never exit by themselves,
  and for comparing what a program did in a fixed span of virtual time.
  Also `stop-after:` in the run file; it needs the supervisor.
- `--capture` writes each guest's stdout to `stdout.<index>` in the
  `--scratch` directory, and `--capture-stderr` its stderr to
  `stderr.<index>` (the supervisor's messages about that guest included;
  without it they come out on ours). The report goes to stderr: `run.*`
  totals, then `p<index>.*` per process.
- `REWRITE_TRACE=file` appends one line per baton switch (from, to, hook
  events issued, site, virtual clock). Diff two of them to find where two
  runs part ways; the schedule hash covers the same values.
- `REWRITE_PARK_SPINS=N` makes a parking thread spin first. It only helps
  when the baton bounces back within microseconds; off by default.

## The run file

```yaml
seed: 7                  # optional; the command line overrides these six
quantum: 1000..10000
heap-size: 32G           # address space of each guest's heap (the default)
mem-hook-rate: 1/16
net-latency: 5ms
stop-after: 30s          # the run is over at this virtual time (default: never)
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
a guest's stack starts. Single-program `rewrite run prog` still inherits.

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
`access`) is allowed; opening them is not.

This guards against configurations that would let hosts share files by
accident, such as an absolute path into another host's directory or a
`../other/data`. It is not a sandbox: paths are checked as text, so a
symlink gets out. Program images are not checked.

Plans and progress: `IMPLEMENTATION_PLAN_REWRITE.md`,
`IMPLEMENTATION_PLAN_MULTIPROC.md`, `TASKS_REWRITE.md`,
`TASKS_MULTIPROC.md`. Results: `docs/REWRITE_RESULTS.md` (single process)
and `docs/MULTIPROC_RESULTS.md` (several processes, virtual network).

## What a guest must be

- An arm64 `MH_EXECUTE` with `LC_FUNCTION_STARTS`, without the hardened
  runtime or library validation (so `DYLD_INSERT_LIBRARIES` is honoured).
- Default link flags are fine. No `-headerpad` is needed.
- Every process in a run must be one of ours: children a guest spawns are
  rewritten on demand. Apple's own binaries (`/bin/sh`, `/usr/bin/curl`,
  `/usr/bin/python3`) ignore `DYLD_INSERT_LIBRARIES` and cannot be guests.
  Homebrew's can: its `curl` and `python3.13 -m http.server` run
  repeatably (see `TASKS_RUNFILE.md`). Rewritten copies of installed
  programs go to `$TMPDIR/rewrite-cache/`, not next to the program.
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
  A lone `rewrite run prog` allows them.
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
- Heap addresses are a function of the seed, and differ between seeds:
  how two blocks compare, and whether `free` then `malloc` returns the
  same block, goes both ways across seeds. A bug that depends on pointer
  order shows on some seeds and replays on those.
- Guests see virtual pids from 100,000 up (above any real pid), in spawn
  order; the launcher is pid 1. A scheduled thread's `pthread_threadid_np`
  is its index in the run plus a billion (kernel thread ids differ from
  run to run; RocksDB mixes one into its DB session ids).
- Where a scheduled thread's mappings land is the run's: thread stacks,
  `pthread_t` blocks and `mmap`s without an address go to a reserved
  region (64 GB at `0x7c_0000_0000`) in schedule order, so the kernel's
  placement of what it maps meanwhile (GCD workers' stacks) cannot move
  them. `pthread_self` is a stack address, and RocksDB seeds its skip-list
  heights from it. The report counts `mappings_placed`.

## Big programs

Stubs are one shared body per program and a four-to-six-word trampoline
per hooked site, so a release build of sui (106 MB of code, 2.5 million
sites) has 45 MB of them. Every site must reach its trampoline, and the
trampoline its target, with one `b` (±128 MB). The stub segment goes
right after `__DATA`, so in a program whose code runs past about 120 MB
the earliest sites cannot reach it: they are left as they are and counted
(`unreachable` in `rewrite scan`, and in the "sites hooked" line; 2.5% of
sui's sites). Code that is not hooked runs without preemption until it
reaches a hook or a blocking call; the run stays deterministic. A callee
too far for a `b` is reached through x16, as a linker's branch island
would. Below `__TEXT` is not an option: the kernel wants `__PAGEZERO` to
cover the low 4 GB and reserves everything up to the first segment.

A function-table entry without a symbol is a constant table in
hand-written assembly (blst keeps its SHA-256 round constants that way),
whose words would be hooked as instructions; such entries are left alone
when the file names at least half of its functions (`unnamed_entries`).

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
rewrite bisect --seed 5 --manifest run.yaml
```

A race may corrupt something long before an assert notices. `bisect`
replays the failing seed and, at a virtual time `t`, gives the scheduler a
new random stream: the run is the failing run until `t` and some other
future after. Once the damage is done nearly every future fails; before,
only as many as ever did. It measures that with `--runs` futures per probe
(default 20, `--jobs` at a time), binary-searches `t` down to
`--resolution` (2 ms of virtual time), and prints the probes, the interval
in which the failure became certain, and the failing run's thread switches
inside it. `--reseed-at T --reseed N` on `rewrite run` replays one such
future. Every random stream starts over at `T`: thread choice and quanta,
injected faults, where heap blocks land, and the entropy guests read. So a
failure decided by pointer order or by a random value is found the same
way; its moment is the allocation or the draw.

## Which lines does a failing run need?

```
rewrite suspects --seed 1 --mem-hook-rate 1 --manifest run.yaml
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
