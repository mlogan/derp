# Site Minimisation: Which Loads and Stores Does the Bug Need?

## Overview

A failing seed with memory hooks on says that some interleaving at some
load or store breaks the program. There may be thousands of hooked loads
and stores. This phase finds a minimal set of them at which the scheduler
must be allowed to switch threads for the failure to reproduce, and names
their source lines.

## What "masking off a rewrite" means

Not removing the stub. The quantum counts hook events, so a binary with
fewer stubs runs a different schedule from the first expiry on, and a
failure that went away would say nothing about the removed site.

Every stub stays and keeps counting. A **masked** site is one where a
switch may not happen: when the quantum runs out there, the supervisor
gives one more event instead of yielding, so the switch falls on the next
hook that is not masked. A run in which no masked site ever saw an expiry
is the unmasked run exactly.

## Design

1. **Site table.** The rewriter knows, for every stub, the address the
   expiry path returns to (what the trace calls `site`), the original
   instruction's address and its kind. It writes them next to the rewritten
   file as `<file>.sites`: `<return address> <site> <branch|call|load|store>`.
2. **Mask.** `REWRITE_MASK=<file>` reaches every guest. Lines are
   `<program> <return address>`. The dylib keeps its program's addresses
   sorted and looks the stub up when a quantum expires (nowhere else, so
   no cost per hook). The variable is set for the reference run too, with
   an empty file: the environment's size decides where a guest's stack
   starts, so every run must have it.
3. **`derp suspects --manifest FILE --seed S --mem-hook-rate R`**
   - Reference run with a trace; it must fail. The signature is the one
     seed bisection uses (`run.failure`).
   - Candidates: the load and store sites at which the reference run
     switched. Every other load and store is masked from here on, which
     leaves the run unchanged. Masks are "everything but the allowed set",
     so a deferred switch can never land on a load or store outside it.
   - If the run still fails with no load or store allowed, the failure
     needs only branch and call switch points: say so and stop.
   - `ddmin` over the candidates: a subset is kept if the run, allowed to
     switch only there, fails the same way. Runs are deterministic, so one
     run decides; the subsets of a round run `--jobs` at a time. The result
     is 1-minimal: dropping any one site loses the failure.
   - Report each site: program, address, load or store, and `atos` on the
     original binary for function, file and line (its dSYM is found next to
     it, as a debugger would).

## Non-goals

- Branch and call sites. The same machinery would do them; they are not
  what was asked for, and a program's control flow makes poor suspects.
- Several failing seeds at once, or probabilities (a masked run is
  deterministic; combining with seed bisection is a later step).

## Test

`lost_update.c`, built with `-g`: two threads bump a counter with an
unsynchronised read-modify-write on one source line, surrounded by decoys
(a mutex-protected counter, per-thread work on shared arrays), and main
aborts on a lost update. The suspects must be on the racy line, none on a
decoy line, and a run allowed to switch only at the suspects must fail.

## Acceptance

The test, the existing suites, clippy clean, and no cost when no mask is
set.
