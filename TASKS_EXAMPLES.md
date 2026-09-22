# Tasks: a library of examples

Plan: `IMPLEMENTATION_PLAN_EXAMPLES.md`. Branch `mlogan-examples`.

## Done (2026-09-22)

- Random devices from the entropy stream (`urandom.c`).
- `gettimeofday` fills its zone argument (UTC).
- Host files copied with their modes.
- Daemons are stopped at the virtual time reached and report; every
  thread of a stopped process is runnable until one takes the baton up
  and ends it (`die_if_stopped`). Tests that expected `signal 9` from a
  daemon now see `stopped`.
- Counter reads (`mrs cntvct_el0`) are virtual: decoder class,
  trampoline with the register number after the call, supervisor entry
  that writes into the saved slot (`cntvct.c`).
- Diagnostics, all off unless asked: `DIAG_TRACE_CLOCK=lo..hi` traces
  clock reads with call chains, `DIAG_TRACE_NET=1` traces virtual-socket
  transfers and the real kqueue's events.
- `examples/redis`: four runs, one hash, one output.
- Signals between guests (delivered at the target's next baton take-up,
  on the Stay path too), `kill(pid, 0)`, `EVFILT_PROC` on virtual pids
  and `EVFILT_SIGNAL` as the run's watches (`sigwatch.c`).
- `semop` as a scheduler wait; keyed `shmget`/`semget` made private and
  removed at the run's end; `shmat` placed in the region; `shm_open`
  names prefixed with the run's pid and unlinked at the end; `getrusage`
  virtual (`sysvsem.c`).
- `setitimer`/`alarm`/`getitimer` as a virtual per-process deadline whose
  SIGALRM wakes a blocked thread (`itimer.c`).
- `DIAG_TRACE_HOOKS=lo..hi`: every interposed call with the quantum's
  remaining hooks and the caller's chain, which is how the last three
  Postgres inputs were found (a drift of 1, then 31 hooks between two
  calls).
- `examples/postgres`: 20 runs, one hash, one output; seed 2 another.

## Done (2026-09-22, later)

- `examples/sqlite` (four Python processes, WAL), `examples/memcached`:
  three runs each, one hash, one output; memcached 10 of 10.
- `examples/go` (a Go server and client): the output repeats every run;
  the schedule hash falls into one of two values (9 and 7 of 16). The
  two traces differ in exactly one line, an expiry of the client's main
  thread at 7 ms that lands one hook earlier or later inside stack
  copying (`getStackMap` against `pcvalue`), and nothing else: not the
  thread order, the clocks, the calls or their results (every interposed
  call in that quantum has the same remaining budget in both, until that
  point). It needs the two Go processes to interact (each alone repeats
  16 of 16), and is not Go's preemption signals (`asyncpreemptoff=1`
  gives the same split), not a store-conditional retry (unhooked now),
  not `madvise` (all succeed), not thread start-up (waiting for the new
  thread to park changed nothing). Open.
- For Go: supervisor stacks for the yield and counter entries (goroutine
  stacks are small), `pthread_kill` between scheduled threads pending
  until the target's take-up, `_exit` writes the report, resolver-free
  lookups answered locally, `socket(AF_INET6)` refused in a run, the
  retry branch after a store-conditional left unhooked, and the run
  starts only once every guest's main thread is parked (two Go
  processes fell into two schedules from the first quantum otherwise;
  what in a guest's start-up tail interfered was not identified, only
  that it did).
- A run whose processes all exited ends at that virtual time, not at
  its stop time.
- `tests/examples_tests.rs` covers all five examples.

## Findings

- A guest cannot run Apple's binaries (`/bin/sh`, `/bin/sleep`, `grep`):
  fat, arm64e, protected. Shell scripts are out; `initdb` and `popen`
  users cannot run as guests. Python clients with pure-Python drivers.
- Redis's monotonic clock is the CPU counter (`monotonic.c`, "ARM
  CNTVCT"); its hash seed is `/dev/urandom`; its zone is
  `gettimeofday`'s. Three real inputs, all now the run's.
- Postgres needed, in the order found: `EVFILT_PROC` on the postmaster's
  virtual pid (the real kqueue rejects it: every child decided the
  postmaster died), signals between guests (SIGURG latches, SIGUSR1),
  `semop` (a backend slept in the kernel with the baton: the run stalled),
  `setitimer` (real SIGALRMs on parked threads), System V keys tried in
  sequence against the machine's leftovers, the `shmat` address, and
  `shm_open` names colliding with a killed run's objects.
- Python's interpreter is a dylib and stays unhooked: its threads switch
  only at interposed calls, and a Python process runs until it blocks
  (the SQLite workers ran one after another until given a pause).
  Rewriting the dylibs a guest loads would close this.
- Go's own linker emits no `LC_FUNCTION_STARTS`; the external linker
  does. Go's `procyield` spins on `cntvct_el0`, which the counter site
  makes virtual.
- `a_reseeded_run_is_the_plain_run_until_the_reseed` failed once in four
  full-suite runs (a reseeded run did not repeat), as `TASKS_REVIEW2.md`
  records once before; not reproduced alone.

## Remaining

- Rewriting the dylibs a guest loads.
- More examples: replication and failover pairs, nginx, Sui's multi-node
  network.
