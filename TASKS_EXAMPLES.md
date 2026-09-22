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

## Findings

- A guest cannot run Apple's binaries (`/bin/sh`, `/bin/sleep`, `grep`):
  fat, arm64e, protected. Shell scripts are out; `initdb` and `popen`
  users cannot run as guests. Python clients with pure-Python drivers.
- Redis's monotonic clock is the CPU counter (`monotonic.c`, "ARM
  CNTVCT"); its hash seed is `/dev/urandom`; its zone is
  `gettimeofday`'s. Three real inputs, all now the run's.
- Postgres: children watch the postmaster with `EVFILT_PROC` on a
  virtual pid, which the real kqueue rejects, so every child decides the
  postmaster died; latches are SIGURG, procsignals SIGUSR1, and the
  supervisor drops both. Needs plan items 6 and 7.
- Python's interpreter is a dylib and stays unhooked: its threads switch
  only at interposed calls.

## Remaining

- Plan items 6 and 7 (postgres).
- Tests for the examples (skipped when not installed).
- More examples.
