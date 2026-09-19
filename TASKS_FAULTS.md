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
- Real pipes and files of a crashed process close when the kernel gets to
  it, in real time; only its virtual sockets close at the crash point.
- A crash lands on a hand-off, so its virtual time is the first yield at or
  after `crash_at`, not `crash_at` exactly.
- Eight crashes at most per hand-off; more wait for the next one.
