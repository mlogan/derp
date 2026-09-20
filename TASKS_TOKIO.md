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

## Second round (2026-09-20, branch `mlogan-tokio-more`)

Added to the guest: `kv sync current` (the same primitives on a
current-thread runtime) and `kv extras [fs|signal|process]` (`tokio::fs`,
`tokio::signal`, `tokio::process`). Tests: both modes equal their native
output on 3 seeds, twice each; `sigchld.c` and `kq_dispatch.c` cover the
supervisor changes directly. 80 of 80 `kv extras` runs gave one schedule
per seed under 8 busy-loop processes.

Found and fixed:

7. **`send` on a kernel socket pair woke nobody.** Only `write` told
   scheduler waiters to look again. Tokio's signal self-pipe is a socket
   pair written with `send`: signals were lost 3 rounds in. `send`,
   `sendto`, `sendmsg` now wake, and `recv`/`recvfrom` on such a socket
   wait in the scheduler.
8. **A signal a guest sends itself** went to whichever thread the kernel
   picked, whenever. `kill(getpid(), sig)` is now `pthread_kill` to the
   calling thread: the handler runs there and then, with the baton.
9. **`SIGCHLD` came at a moment of real time.** Its handler (tokio's wakes
   the reaper) made the schedule depend on it: 2 schedules in 12 runs under
   load. The guest's handler is now kept by the supervisor
   (`signals.rs`, `sigaction`/`signal` interposed for this signal only),
   the kernel's delivery is dropped, and the handler runs on the parent's
   next thread to take up the baton after the death is recorded.
10. **A contended lock inside the supervisor showed in the guest's
    schedule** (found with `kv addrs`, below). The supervisor's own
    allocations use libmalloc, whose internal lock is an `os_unfair_lock`.
    When two of our threads collide on it in real time (the path checker
    growing a vector while a new thread allocates during start-up; the
    trace writer freeing its line after it has handed the baton on), the
    loser's `__ulock_wait2` reaches our interposer. That treated any
    registered owner as "parked with the lock, pass the baton on" and
    yielded: an extra switch, or a changed order of threads, from a
    collision that has nothing to do with the guest. Now (`unfair_wait`):
    only the baton holder's waits count; an owner that is one of ours but
    not parked (`ThreadRec::in_park`) is running in real time and is
    waited out in the kernel; an owner seen parked only counts if the lock
    word still names it afterwards. Measured on seed 2 of `kv addrs` under
    32 spinning processes with tracing on: before, about 1 run in 40
    diverged; after, 15 in 3,000, all of the kind under "Open".
    My first explanation (frees by threads outside the schedule) was wrong
    for tokio: the report counts such frees (`heap_leaked_blocks`) and
    every tokio mode shows 0. The leak rule stays for the curl scenario,
    where a GCD thread frees one 144-byte block per run.
11. `EV_DISPATCH` is modelled (checked against the kernel's answers).

Decided with Mark: **waits on descriptors from outside the run stay
unsupported**; they could not be repeated. Such a wait now says so once in
the log instead of failing silently.

## The supervisor has its own allocator (2026-09-20)

Finding 10 treated the symptom. The cause was that the supervisor's Rust
allocations went to libmalloc, so our threads shared libmalloc's locks with
each other and with the system libraries, in real time. `alloc.rs` now has
a second heap (`OWN`, 1 GB of address space at 0x76_0000_0000, the same
size-class code as the guest's heap, its own spin lock) behind
`#[global_allocator]`. The guest's `free`, `realloc` and `malloc_size`
accept its pointers, in case a guest is ever handed one; Rust-side frees of
foreign pointers go to libc. It initialises itself without allocating or
logging, and is force-unlocked in a `fork` child like our other locks.

Also fixed: the thread-exit hook's join wake was counted as a wake from
outside the schedule (libpthread clears our key before calling the hook),
which marked every threaded process as having outside threads. An idle run
then polled for 30 s instead of reporting its deadlock. `outside_wakes` is
0 again for a guest without GCD threads.

Measured, seed 2 of `kv addrs`, 32 spinning processes, tracing on:

| build | divergent runs | runs with a contended `__ulock_wait2` |
|---|---|---|
| before this round | about 1 in 40 | about 1 in 40 |
| interposer fix only | 15 of 3,000 | 30 of 3,000 |
| own allocator | 0 of 3,000 | 0 of 3,000 |

and the test, with its own load and no tracing: 0 of 4,000 (2,000 per
seed), then 0 of 1,000 after the exit-hook change. The 96-byte placement
difference and the one hang listed as open are gone with the collisions;
what exactly placed that block differently was never identified.
`tokio_heap_addresses_are_a_function_of_the_seed` runs by default again
(8 runs per seed; `REWRITE_STRESS_RUNS` to hunt).

Cost: none measurable. `loops 3`, release: 1.74x branch hooks, 2.26x with
memory hooks at 1/16 (1.73x and 2.21x before). Server with two clients at
200 rounds, release: 0.609 s against 0.612 s with libmalloc. The stubs and
the hand-off path never allocated; the supervisor allocates a few small
blocks per intercepted system call.

## Limits

- A kqueue or poll set mixing guest descriptors with outside-world ones is
  only woken by the guests' side. Unsupported by decision; logged.
- Other signals than `SIGCHLD` that the kernel raises on its own
  (`SIGALRM`, `SIGPIPE` from a kernel pipe, `SIGIO`) still arrive in real
  time. `tokio::signal` for signals sent from outside the run is input.
- Blocks freed by threads outside the schedule are leaked.
- Rwlock waiters have no writer preference; the next holder is the
  scheduler's draw.

## Test summary

Full suites pass (4 consecutive runs), clippy clean.
