# Tokio Guest: Progress

Plan: `IMPLEMENTATION_PLAN_TOKIO.md`. Branch `mlogan-tokio-kv`.

## Done (2026-09-20)

- **`tests/programs/kv/`**: a cargo package outside the workspace
  (its own `Cargo.lock`; the tests build it once with `cargo build
  --locked` into `target/debug/rewrite-tests/kv-build`).
  - `kv sync`: `Mutex`, `RwLock`, `Semaphore` (+`acquire_many`), bounded
    `mpsc` into a plain thread with `oneshot` replies, unbounded `mpsc`
    from `spawn_blocking`, `broadcast` with lag, `watch`, `Notify`,
    `Barrier`, `OnceCell`, `JoinSet`, `select!`, `timeout`, `interval`,
    `sleep`, on a 3-worker runtime. Self-checking; output depends on data
    only.
  - `kv server PORT CLIENTS [LOG]`: connections send writes over a bounded
    `mpsc` to one processing thread (`blocking_recv`, `blocking_write`),
    which logs to disk before applying and answering; reads take the
    `RwLock`; stats under a `Mutex`; a `Semaphore` bounds requests in
    flight; `broadcast` feeds `SUBSCRIBE`; `watch` carries shutdown;
    `Notify` ends the accept loop; `HASH` runs in the blocking pool.
  - `kv client HOST PORT ID N`: current-thread runtime, three connections
    behind a `Barrier` plus a subscriber, every reply checked, reconnect
    and resend on a dropped connection (`INCR` carries a request id).
- **Tests** (`tests/tokio_tests.rs`), all at quantum 50..500 for many more
  switch points, each run twice per seed for output and schedule hash:
  `kv sync` equals its native output on 4 seeds with 4 different
  schedules; server + 2 clients on 3 seeds; the same with the server
  crashed 3 times and restarted, total still exact, clients reconnect.
- `tests/programs/rwlock.c` in `threads_tests`: pthread rwlock readers
  never see half a write; `pthread_mutex_trylock` never blocks.

## What it found in the supervisor

1. **A kqueue holding only a user event waited in the kernel**, baton in
   hand. mio's waker is `EVFILT_USER`, triggered by another *scheduled*
   thread, which never ran. `kevent` waits are now the scheduler's unless
   every registration on the kqueue belongs to the outside world.
2. **`EV_RECEIPT` was not modelled**; mio uses it for every registration
   and expects an immediate answer. Receipts for virtual sockets are
   synthesized, the kernel's passed through.
3. **Duplicated kqueue descriptors**: mio registers through one
   `F_DUPFD_CLOEXEC` copy and waits on another; our registry was per
   descriptor. `kqueue()` is interposed and duplicates share one entry.
4. **`pthread_cond_timedwait_relative_np`** (Rust's
   `Condvar::wait_timeout`, used by tokio's blocking pool) was not
   interposed: a 10 s real sleep with the baton.
5. `pthread_rwlock_{rd,wr}lock/unlock` were not interposed (found by
   reading, not by tokio: Rust's `RwLock` does not use them).
6. The deadlock report now lists each blocked thread and what it waits for.

Not a supervisor bug: macOS kills a process whose executable was
overwritten in place (cached code signature); the test helper copies to a
new file and renames.

Found in the guest, not the supervisor: with memory hooks on, less work is
done per virtual millisecond, so at one seed the first crash came before
any change had been broadcast and the client's subscriber, which ended at
the first dropped connection, had seen none. It now resubscribes until the
server ends the stream. The tests run memory-hook variants (1/16, 1/4, 1).

## Limits

- A kqueue mixing virtual sockets with outside-world registrations is
  still only woken by the virtual side (unchanged, `TASKS_RUNFILE.md` §6).
- Rwlock waiters have no writer preference; the next holder is the
  scheduler's draw.
- `EV_DISPATCH` is still unmodelled.

## Test summary

Full suites pass (4 consecutive runs), clippy clean.
