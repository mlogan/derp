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
  only at interposed calls.

## Remaining

- More examples.
