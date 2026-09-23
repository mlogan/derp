# Tokio Guest: a Key-Value Server on the Async Runtime

## Overview

Our test guests use a pthread mutex, one condvar timed wait, one GCD
semaphore and std `mpsc`. Nothing exercises an async runtime: worker
threads that park on condvars, a kqueue reactor with a user-event waker, a
blocking pool, timers, and the `tokio::sync` primitives built on atomics.
This phase adds such a guest and makes it run repeatably.

## Goals

1. `tests/programs/kv/`: a cargo package (outside the workspace)
   building one binary, `kv`, on tokio.
2. `kv sync`: one process, no sockets. Multi-thread runtime exercising
   `mpsc`, `oneshot`, `broadcast`, `watch`, `Mutex`, `RwLock`, `Semaphore`,
   `Notify`, `Barrier`, `OnceCell`, `JoinSet`, `select!`, `spawn_blocking`,
   `time::{sleep, timeout, interval}`. Self-checking; prints a summary.
3. `kv server PORT` / `kv client HOST PORT ID N`: a line-protocol
   key-value server over the virtual network.
   - connection tasks send writes over an `mpsc` to a dedicated processing
     thread (`blocking_recv`), replies come back on `oneshot`;
   - reads take an `RwLock` on the map directly; stats sit under a `Mutex`;
   - a `Semaphore` bounds in-flight requests; `broadcast` feeds
     subscribers; `watch` carries shutdown; `Notify` signals a flush.
   - clients run concurrent tasks, check every reply, and a final shared
     counter must equal the number of `INCR`s sent.
4. Tests: each mode passes under the supervisor, output and schedule hash
   repeat for a seed, different seeds give different schedules, and the
   server survives fault injection (crash + restart, clients reconnect).
5. Fix whatever in the supervisor this turns up.

## Non-goals

- Tokio `fs`, `process`, `signal`.
- Performance work.
- Guests using GCD (still refused).

## Expected trouble

- mio's waker is `EVFILT_USER` on a kqueue that also holds virtual sockets;
  our `kevent` emulation must treat a trigger from a scheduled thread as a
  wake (known gap: mixed virtual/external sets).
- std `Mutex`/`Condvar`/`thread::park` on macOS: whichever of pthread,
  `os_unfair_lock` or ulock they use must block in the scheduler.
- `pthread_rwlock_*`, `pthread_mutex_trylock`, `pthread_cond_timedwait_relative_np`
  are not interposed.
- The blocking pool spawns threads on demand and lets them time out.

## Order

1. Package builds offline; `kv sync` native.
2. `kv sync` supervised and repeatable.
3. Server and client native, then supervised over the virtual network.
4. Fault injection scenario.
5. Tests, docs, review.

## Acceptance

All of goal 4, the full suites green, clippy clean.
