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
- `--manifest FILE` lists virtual hosts and the processes on each; hosts
  get `10.0.0.1` upward and are reachable by name. `--net-latency 5ms`
  delays traffic between different hosts in virtual time.
- `--capture` writes each guest's stdout to `stdout.<index>` in the
  `--scratch` directory. The report goes to stderr: `run.*` totals, then
  `p<index>.*` per process.
- `REWRITE_PARK_SPINS=N` makes a parking thread spin first. It only helps
  when the baton bounces back within microseconds; off by default.

Plans and progress: `IMPLEMENTATION_PLAN_REWRITE.md`,
`IMPLEMENTATION_PLAN_MULTIPROC.md`, `TASKS_REWRITE.md`,
`TASKS_MULTIPROC.md`. Results: `docs/REWRITE_RESULTS.md` (single process)
and `docs/MULTIPROC_RESULTS.md` (several processes, virtual network).

## What a guest must be

- An arm64 `MH_EXECUTE` with `LC_FUNCTION_STARTS`, without the hardened
  runtime or library validation (so `DYLD_INSERT_LIBRARIES` is honoured).
- Default link flags are fine. No `-headerpad` is needed.
- Every process in a run must be one of ours: children a guest spawns are
  rewritten on demand. Platform binaries (`/bin/sh`, coreutils) are out of
  scope.

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
