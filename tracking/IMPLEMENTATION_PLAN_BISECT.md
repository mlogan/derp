# Seed Bisection: When Did the Run Go Wrong?

## Overview

A failing seed says that a run fails and replays it, but not when the
failure was decided. A race corrupts something at one moment; the assert
that notices may run much later. Bisection finds the moment.

Replay the failing seed, and at virtual time `t` replace the state of the
scheduler's random stream. Up to `t` the run is the failing run exactly;
after `t` it is some other future. If the damage is done before `t`, the
failure is baked in and nearly every such future fails. If it is not, the
failure is only as likely as it ever was. So the probability of failure as
a function of `t` steps up where the damage is done, and a binary search
over `t` finds the step. Each probe is many runs with different
replacement streams, because what is measured is a probability.

## Goals

1. `--reseed-at NS --reseed N` on `derp run`: at the first hand-off at
   or after virtual time `NS`, the scheduler's stream (thread choice and
   quanta) and the fault stream are reseeded from `N`. The run is identical
   to the plain run until then, and a function of (seed, NS, N) after.
2. The report says when each process died in virtual time
   (`p<i>.died_at`).
3. `derp bisect --manifest FILE --seed S [--runs N] [--jobs J]
   [--resolution DUR]`:
   - runs the seed, takes the failure's signature (which run-file entry
     died and how) and its time `T`;
   - measures the base rate with a reseed at the very start;
   - binary-searches `[0, T]`: a probe is `N` reseeded runs at `t`, run `J`
     at a time, and counts runs with the same signature; above the midpoint
     between base rate and 1 means "already decided";
   - prints every probe (`t`, failures of runs), the interval where the
     failure becomes likely, and the reference run's switches inside it.
4. A guest with a latent bug to prove it on: an unsynchronised
   read-modify-write at a known moment, detected by an assert much later.
   The guest prints the window in virtual time; the test checks that the
   interval bisection reports lies at the window and not at the assert.

## Non-goals

- Reseeding the per-process streams (heap layout, entropy). Their state is
  in each guest; a later step. Until then a failure that depends only on
  heap layout looks decided from the start.
- Single-program runs (`derp run prog`): bisection takes a run file. A
  one-process run file is three lines.
- Failures that are not an exit status (wrong output). The signature could
  later be a user-supplied check command.

## Notes on the statistics

- With base rate `p0` and `N` runs per probe, a probe before the damage
  sees about `p0 N` failures and one after sees about `N`. The threshold is
  halfway. `N = 20` separates `p0 <= 0.5` from 1 comfortably; the closer
  `p0` is to 1 the less there is to find, and bisect says so and stops.
- The step may be gradual (several contributing events). The table of
  probes is printed so that this is visible; the search still returns the
  place where the probability crosses the threshold.
- Virtual time moves 1 ms per yield, so the default resolution is 2 ms.

## Order

1. `reseed_at` in the shared state, flags, `died_at`; test that the prefix
   is identical and the suffix differs.
2. `latent.c` and a scan for a failing seed.
3. `bisect` command with parallel probes.
4. Test, docs.
