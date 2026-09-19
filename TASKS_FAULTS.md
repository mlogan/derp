# Process Fault Injection: Progress

Tracks `IMPLEMENTATION_PLAN_FAULTS.md`. Branch `mlogan-fault-injection`.

## Done (2026-09-19)

- **Run file**: on a process, `restart: never | on-failure | always`,
  `restart-delay: 50ms..150ms` (a range drawn per death; default 100 ms and
  never zero), `max-restarts: N`, `crash: { every: 100ms..300ms, times: 3 }`.
  Bad values and unknown keys are errors.
- **The scheduler crashes processes, in virtual time** (`shared.rs`). Each
  process record carries its `Faults` and a `crash_at`, drawn from a
  separate RNG stream so that faults do not shift thread choice. The check
  runs in `pick()`, on every hand-off, and an idle run jumps to the next
  crash as it does to the next timer.
- **`State::crash` is the whole death, under the lock**: threads retired,
  virtual sockets released (peers see EOF or EPIPE from that point), the
  parent's `waitpid` woken, the restart registered. The baton holder then
  sends `SIGKILL` (`take_kills`); a process that crashes itself dies after
  the baton is safely elsewhere. `kill` between guests uses the same path,
  which also fixed its peers seeing EOF only when the launcher noticed.
- **A restart is a new process** (new index, new virtual pid, same host)
  whose main thread is blocked on `RESTART_KEY` until `death + delay`. The
  process is down for that time: connections to it are refused. The
  launcher spawns the real process when it sees the death
  (`respawn` in `launch.rs`); how long that takes cannot affect the
  schedule. Deaths a process causes itself are registered in
  `process_died`, deterministic there because the process held the baton.
- The host directory survives, captured stdout is appended to, and the
  run's exit status is the *last* life of each run-file entry.
  `run.crashes_injected`, `run.restarts`, `p<i>.entry`.
- `yield_baton_as` lost its duplicated resume logic on the way (`After`).

## Tests (`ping_pong.c`)

- Server crashed three times, restarted each time: the client gets all 120
  pongs in order from lives 1 to 4, reconnects three times, and measures
  each outage on the virtual clock at 50 to 200 ms (the delay plus startup).
  The server's `lives` file in its host directory reads 4. Crash moments
  differ across seeds.
- Client crashed twice: it resumes from its `progress` file (at pings 28
  and 59 for seed 1) and finishes; the run succeeds though two lives died.
- `restart: never` and `max-restarts: 1` leave the process dead and the run
  failed, with the expected crash and restart counts.
- 100 identical runs of each scenario; 40 byte-identical schedule traces of
  the server scenario under 8 busy-loop processes.

## Limits

- Only processes the launcher starts from the run file are restarted; a
  guest's own children are their parent's to restart.
- A run holds 1024 processes, and every life is one: a run that would
  need more ends with an error saying so. An unlimited `crash:` on a run
  that is otherwise deadlocked keeps it going until then.
- A crash lands on a hand-off, so its virtual time is the first yield at or
  after `crash_at`, not `crash_at` exactly.
- A process killed while one of its unscheduled threads holds the shared
  lock wedges the run (GCD guests are refused, so nothing does this today).
- The crashed process's own last `REWRITE_TRACE` line can be lost; the
  schedule hash is unaffected.

## Deviations from the plan

- `restart-delay` became a range with a 100 ms default and may not be zero
  (a restart is never instantaneous).
- The report names a life's run-file entry (`p<i>.entry`), not
  `restart_of`.

## Review (2026-09-19)

Three reviewers; every confirmed finding is fixed and has a test.

- **Hang**: more than eight crashes due at once spun forever under the
  lock. The kill queue is gone: `take_kills` scans for `killed &&
  !signalled`.
- **Determinism**: the launcher's `process_died` repeated a crashed
  process's wakes at a moment of real time. It now only records the exit;
  instead whoever takes up the baton next waits (in real time, the
  schedule standing still) until the victim is really dead and then wakes
  I/O waiters once (`settle_deaths`). Real pipes and locks of a crashed
  process are thereby released at a fixed point too.
- **Determinism**: a crash due before the replacement had attached waited
  for the attach. Crashes now land on schedule whatever the state; a life
  that finds itself crashed while attaching goes by itself.
- `kill` of a process that is down between lives no longer reaches
  `kill(0, …)`; an already-crashed target is `ESRCH`.
- Full tables: `will_restart` and `register_restart` share one predicate
  and the run ends with an error instead of a false deadlock.
- Fault keys without the supervisor are an error; durations over a year
  are rejected; a replacement that dies before it is watched is handled
  before the first baton; captured stdout is always opened `O_APPEND`.
- Simplifications taken: `restart_of`, `Tracked::ours` and the pending-kill
  tuple removed, `pending_crash` shared. Deferred: one coordinator call per
  death, a `Supervision` struct for the launcher loop, a common
  `retire` for `crash`/`process_died`, manifest normalisation, test
  scaffolding helpers.
