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
- `--capture` writes each guest's stdout to `stdout.<index>` in the
  `--scratch` directory. The report goes to stderr: `run.*` totals, then
  `p<index>.*` per process.
- `REWRITE_TRACE=file` appends one line per baton switch (from, to, hook
  events issued, site, virtual clock). Diff two of them to find where two
  runs part ways; the schedule hash covers the same values.
- `REWRITE_PARK_SPINS=N` makes a parking thread spin first. It only helps
  when the baton bounces back within microseconds; off by default.

## The run file

```yaml
seed: 7                  # optional; the command line overrides these four
quantum: 1000..10000
mem-hook-rate: 1/16
net-latency: 5ms
hosts:                   # in order: 10.0.0.1, 10.0.0.2, ...
  - name: alpha
    processes:
      - [server, --port, 8080]          # argv verbatim
      - client alpha 8080               # or a line, split on whitespace
      - argv: [worker, "two words"]     # or a map, with an environment
        env: { MODE: fast }
```

A process written as a map may also say `daemon: true`: a server that never
exits. When every other process of the run file has exited, the launcher
kills what is left and the run is over.

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
- Guests see virtual pids from 100,000 up (above any real pid), in spawn
  order; the launcher is pid 1.

## Rewritten binaries only run under the supervisor

The stubs do not address anything in the image. They reach the quantum
counter and the scheduler entry point in a fixed region at
`0x78_0000_0000` that the supervisor dylib maps at startup (one private
page per process, then the run's shared scheduler state). A default-linked
binary has header room for one new segment, which holds the stub code;
there is nowhere to put a writable word. Running `prog.rw2-…` directly
faults at the first hooked branch.

`--no-supervisor` and `bench` still inject the dylib, in a passive mode
that maps the region and schedules nothing. It adds under 1 ms of startup.

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
