# Implementation plan: a library of examples

## Overview

Run real servers with real clients under the supervisor, repeatably, and
keep each as an example under `examples/`. Every example is a run
file, a client and a README line; a test runs each twice and compares.
What the systems need from the supervisor along the way is the real work.

## Goals

- `examples/redis`: redis-server and a multi-threaded Python client.
- `examples/postgres`: postgres (multi-process) and a Python client.
- Whatever each needs: inputs the supervisor did not yet own.
- A test per example, skipped when the software is not installed.

## Non-goals

- Rewriting dylibs (Python's interpreter runs unhooked between calls).
- Clients in shell: guests cannot run Apple's binaries.
- Sui's multi-node run file (separate phase).

## Specification

1. **Random devices**: reads of `/dev/urandom` and `/dev/random` come
   from the entropy stream, like `getentropy`. Redis seeds its hash
   tables from the device.
2. **`gettimeofday`'s zone**: filled with UTC (Redis takes its zone
   from it on macOS; unfilled it printed 1970).
3. **Host files keep their modes**: postgres refuses a data directory
   others can read.
4. **Daemons are stopped, not killed**, when every other process is
   done: the launcher arms a stop at the virtual time reached, every
   thread of a stopped process is made runnable, and the first to take
   the baton up writes the report and ends the process. A grace timer
   (5 s real) kills what never takes the baton up.
5. **The CPU's counter is the virtual clock**: `mrs xN, cntvct_el0` and
   `cntpct_el0` become sites; the trampoline calls a second supervisor
   entry (`COUNTER_ENTRY_OFFSET` in the fixed region) with the register
   number in the word after the call, and the entry drops the virtual
   clock, in ticks, into that register's saved slot. Cache tag `.rw5-`.
6. **Signals between guests** (postgres): `kill(vpid, sig)` for any
   signal is recorded against the target process and delivered on the
   target's next baton take-up (`pthread_kill` to self, so the handler
   runs with the baton), waking a blocked thread if none runs;
   `kill(vpid, 0)` answers liveness. `EVFILT_SIGNAL` and `EVFILT_PROC`
   registrations on virtual pids are the run's: readiness from the
   pending-signal counts and the process table.
7. **Whatever else postgres needs** once signals work (SysV semaphores
   are the likely next).
8. Examples README, per-example tests, `.gitignore` for the venv and
   the data directory.

## Order

1 to 5 (done), README and tests, then 6, 7, then more examples.

## Acceptance

- Each example: three runs, one hash and one output; another seed,
  another hash.
- Tests for 1, 3, 4, 5, 6 with small C programs.
- `cargo test --release --workspace` and `cargo clippy` clean.
