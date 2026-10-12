# Thread pauses and PCT: results

What pauses and the PCT scheduler cost and what they find, measured to
choose their defaults. Release build, Apple M-series laptop, October 2026.
The script that produced the tables ran each workload with `derp run`.

## Cost

Wall time, median of seeds 1–3. PCT's horizon is the baseline run's
virtual length.

| workload | baseline | pause 1/1000 | pause 1/200 | pause 1/50 | pct d3 | pct d3 + pause 1/200 |
| --- | --- | --- | --- | --- | --- | --- |
| two `loops.c` | 0.28s | 0.20s | 0.19s | 0.19s | 0.19s | 0.19s |
| redis example | 0.47s | 0.45s | 0.45s | 0.45s | 0.45s | 0.44s |
| postgres example | 0.77s | 0.83s | 0.86s | 0.80s | 0.81s | 0.89s |
| memcached example | 0.71s | 0.80s | 0.83s | 0.77s | 0.95s | 0.92s |

The differences are within what one run differs from the next: a pause
does no work, it only reorders it, and every example produced its
expected output under every setting.

## Bugs found

Seeds of 1..50 whose run shows the bug. Pauses are `10us..100ms`; PCT's
horizon is about each program's run (lease 100ms, order 100us, atomicity
50us, race 10ms).

| program | random | pause 1/200 | pause 1/50 | pct d1 | pct d2 | pct d3 | pct d3 + pause 1/200 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `lease.c` (a stall longer than a lease) | 0/50 | 21/50 | 25/50 | 16/50 | 12/50 | 10/50 | 27/50 |
| `order.c` (depth 1) | 31/50 | 31/50 | 31/50 | 23/50 | 24/50 | 25/50 | 24/50 |
| `atomicity.c` (depth 2) | 15/50 | 14/50 | 13/50 | 1/50 | 16/50 | 17/50 | 17/50 |
| `race.c`, memory hooks 1/16 | 3/50 | 3/50 | 3/50 | 0/50 | 0/50 | 0/50 | 2/50 |

- Pauses find what the uniform pick cannot: a thread left out for
  milliseconds while others run. They cost nothing measurable and change
  nothing on the other programs, so they are on by default at 1/200.
- PCT finds the depth-2 bug about as often as the uniform pick, and the
  lease bug without pauses, but only with a horizon close to the run's
  length, and it does worse on the easy bugs: under strict priorities a
  thread runs its whole loop alone, where `race.c` needs threads to cut
  into one another. `random` stays the default.
- At depth 1, PCT has no change point, so atomicity.c's window is never
  split (1/50).
