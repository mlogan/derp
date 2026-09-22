# Examples: real systems under the supervisor

Each directory holds a run file and a client for one system, meant to run
repeatably: the same seed gives the same schedule hash and the same client
output every time, and another seed another schedule.

    rewrite run --manifest examples/redis/run.yaml --capture --scratch /tmp/rd
    cat /tmp/rd/stdout.1

## Setting up

The servers come from Homebrew and the clients are Python with pure-Python
drivers, so that a client is one process (a guest cannot run Apple's own
binaries such as `/bin/sh` or `/bin/sleep`: they are fat, arm64e and
protected, so shell scripts are out).

    brew install postgresql@17 redis python@3.13
    /opt/homebrew/opt/python@3.13/bin/python3.13 -m venv examples/.venv
    examples/.venv/bin/pip install pg8000 redis
    examples/postgres/setup.sh        # initdb, once, natively

The run files copy the venv's `site-packages` into the client's host
directory and point `PYTHONPATH` at it.

## What each one shows

- `redis/`: a server and a four-thread client doing pipelined increments,
  hashes, a queue and deletes. Redis reads the CPU's counter register for
  its monotonic clock and seeds its hash tables from `/dev/urandom`; both
  are the run's now.
- `postgres/`: a server (its data directory prepared natively, since
  `initdb` shells out) and a four-thread client doing updates, deletes
  and inserts in transactions. Not yet repeatable: see the plan.

## Limits worth knowing

Only executables are rewritten, not the dylibs they load: Python's
interpreter lives in a framework dylib, so a Python client's threads
switch only at interposed calls (I/O, locks, sleeps), never inside its
bytecode loop. That is deterministic, but coarse.
