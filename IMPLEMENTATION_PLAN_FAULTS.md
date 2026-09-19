# Process Fault Injection and Restart

Branch `mlogan-fault-injection`, off `main`. Progress in `TASKS_FAULTS.md`.

## Goal

Crash processes at seeded moments, restart them as the run file says, and
show with a ping-pong pair that the survivors carry on and reconnect. A
crash schedule is part of the run: the same seed crashes the same process
at the same point of the same schedule.

## Run file

```yaml
hosts:
  - name: server
    processes:
      - argv: [ping_pong, pong, 7000]
        daemon: true
        restart: on-failure      # never (default) | on-failure | always
        restart-delay: 20ms      # virtual time between death and the new life (default 10ms)
        max-restarts: 10         # default: no limit
        crash:
          every: 50ms..200ms     # virtual time into each life, drawn per life
          times: 3               # crashes to inject over the run (default: no limit)
```

`on-failure` restarts after a signal or a non-zero exit (an injected crash
is a `SIGKILL`); `always` also after `exit(0)`.

## Design

- **The scheduler decides, in virtual time.** Each process record carries
  its fault settings and a `crash_at`. Whoever gives up the baton checks,
  under the lock, whether a crash is due; an idle run jumps to the next
  crash as it does to the next timer. Crash times come from their own RNG
  stream (seeded from the run seed), so adding faults does not shift thread
  choice.
- **A crash is complete in the shared state before the signal goes out**:
  the victim's threads are retired, its virtual sockets released (peers see
  EOF or EPIPE from that point, not when the launcher notices), its parent's
  `waitpid` woken. Then the baton holder sends `SIGKILL`; a process that
  crashes itself does so after passing the baton on. A victim that has not
  attached yet is crashed on a later check. `kill` between guests goes
  through the same path.
- **A restart is a new process registered at the moment of death**, with a
  new index and virtual pid, on the same host, whose main thread is blocked
  until `death + restart-delay`. The launcher spawns the real process when
  it sees the death; how long that takes cannot matter, because the thread
  is not runnable before its time and a baton handed to a process that has
  not attached yet simply waits for it. Deaths the process causes itself
  (exit, abort) are registered by the launcher's death handling, which is
  deterministic there because the dying process held the baton.
- **The host directory outlives the process**, so a restarted process finds
  what its earlier lives wrote. Captured stdout is appended to.
- A run is over when no process of a non-daemon run-file entry is alive or
  waiting to restart.

## Report

`run.crashes_injected`, `run.restarts`; per process `restart_of=<index>`.

## Tests (`ping_pong.c`)

1. The server crashes three times and restarts; the client reconnects and
   still gets every pong, in order. The server counts its lives in a file in
   its host directory.
2. The client crashes twice and restarts; it keeps its progress in a file
   and resumes where it was.
3. Both: same output and schedule hash on repeated runs of a seed,
   different crash points across seeds, 100-run check by hand.
4. Run-file validation of the new keys; `never` leaves a crashed process dead.

## Non-goals

Crashing at a chosen program point, partial failures (hung process, slow
disk), network faults (the simulator's job), crashing the launcher.
