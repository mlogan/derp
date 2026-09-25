# Examples: real systems under the supervisor

Each directory holds a run file and a client for one system, meant to run
repeatably: the same seed gives the same schedule hash and the same client
output every time, and another seed another schedule.

    derp run --manifest examples/redis/run.yaml --capture --scratch /tmp/rd
    cat /tmp/rd/stdout.1

## Setting up

The servers come from Homebrew and the clients are Python with pure-Python
drivers, so that a client is one process (a guest cannot run Apple's own
binaries such as `/bin/sh` or `/bin/sleep`: they are fat, arm64e and
protected, so shell scripts are out).

    brew install postgresql@17 redis memcached go python@3.13
    /opt/homebrew/opt/python@3.13/bin/python3.13 -m venv examples/.venv
    examples/.venv/bin/pip install pg8000 redis pymemcache
    examples/postgres/setup.sh        # initdb, once, natively
    (cd examples/go && go build -ldflags=-linkmode=external -o bin/kvgo .)

The run files copy the venv's `site-packages` into the client's host
directory and point `PYTHONPATH` at it.

## What each one shows

- `redis/`: a server and a four-thread client doing pipelined increments,
  hashes, a queue and deletes. Redis reads the CPU's counter register for
  its monotonic clock and seeds its hash tables from `/dev/urandom`; both
  are the run's now.
- `postgres/`: a server (its data directory prepared natively, since
  `initdb` shells out) and a four-thread client doing updates, deletes
  and inserts in transactions. Postgres is many processes that signal
  each other, watch the postmaster's pid, sleep on System V semaphores,
  time statements with `setitimer`, and claim shared memory by keys and
  names; each of those is the run's now.

- `sqlite/`: four Python processes on one host sharing a database in WAL
  mode, each inserting and updating in transactions with a busy timeout:
  file locks, the write-ahead log and its shared-memory index between
  guests. Each worker pauses a millisecond between rounds, since the
  interpreter runs unhooked and would otherwise run to the end unpaused.
- `memcached/`: a server with four worker threads and a four-thread
  Python client doing sets, increments, appends and deletes.
- `go/`: a key-value server and an eight-goroutine client, both Go. Go's
  own linker emits no `LC_FUNCTION_STARTS`, so the binary is built with
  the external linker; Go's resolver is told to use the system's
  (`GODEBUG=netdns=cgo`) so that it learns the run's host names. The
  runtime's goroutine stacks, signal-based preemption, kqueue polling
  and counter-based spin waits are all covered. Support is partial: the
  output repeats, the schedule hash takes one of two values (one quantum
  ends one hook apart), which `tracking/TASKS_EXAMPLES.md` records with the next
  steps; set aside for now.

A Sui cluster after sui-operations' Antithesis compose file (four
validators, two fullnodes and the `stress` client) lives in the sui
repository, `scripts/derp/`: its `run.sh` builds the binaries with
`derp cargo` from that checkout (`DERP_DIR` names this one) and runs
its run file. `tracking/TASKS_SUI_CLUSTER.md` has the numbers.

## Limits worth knowing

Only executables are rewritten, not the dylibs they load: Python's
interpreter lives in a framework dylib, so a Python client's threads
switch only at interposed calls (I/O, locks, sleeps), never inside its
bytecode loop. That is deterministic, but coarse.
