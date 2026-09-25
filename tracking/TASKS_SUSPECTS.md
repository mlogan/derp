# Site Minimisation: Progress

Plan: `IMPLEMENTATION_PLAN_SUSPECTS.md`. Branch `mlogan-site-bisect`.

## Done (2026-09-20)

- **Site table** (`rewrite.rs`, `cache.rs`): for every stub, the address
  its expiry path returns to (the `site` of a schedule trace), the original
  instruction's address and its kind (`branch`, `call`, `load`, `store`;
  load or store from bit 22 of the displaced word). Written as
  `<rewritten file>.sites`, moved into the cache before the file itself so
  that a cached rewrite always has one. The cache tag is `.rw3-` so older
  cached rewrites are redone.
- **Mask** (`sched.rs`): `REWRITE_MASK=<file>` with lines
  `<program> <return address>`; the launcher hands it to guests like
  `REWRITE_TRACE`. When a quantum runs out at a masked stub the supervisor
  installs a quantum of one and returns, so the switch falls on the next
  hook. Looked up only on expiry: no cost per hook. Addresses are taken
  before any slide, found for the executable itself (image 0 is the
  inserted library).
- **`derp suspects --manifest FILE --seed S --mem-hook-rate R`**
  (`src/suspects.rs`):
  1. reference run with a trace and an empty mask; it must fail;
  2. candidates: the loads and stores the run switched at. Masks are always
     "every site of the kind under study except the allowed", so a deferred
     switch cannot land on one outside the set;
  3. sanity: allowing exactly the candidates must reproduce the failure
     (it is the same run); allowing none tells whether memory switch points
     matter at all;
  4. `ddmin` over the candidates, each round's runs `--jobs` at a time, one
     deterministic run per subset;
  5. **if no load or store is needed**, the same over branches and calls,
     in the run that has every load and store masked; if none of those
     either, "blocking calls are enough";
  6. `atos -o <original binary>` for function, file and line; the dSYM is
     found next to the binary.
- **`tests/programs/lost_update.c`**: one unprotected `racy = racy + 1`
  among a mutex-protected counter and a shared array.

## Results

```
$ derp suspects --seed 1 --mem-hook-rate 1 --manifest lu.yaml
reference: seed 1 fails (entry 0: signal 6); it switched at 4 of 27 hooked loads and stores
  still fails with 2 sites
  still fails with 1 sites
1 of 4 sites are needed (7 runs):
  lost_update 0x1000006bc store worker (in lost_update) (lost_update.c:19)
```

Seeds 6 and 12 give the same store on line 19, the racy line, in 7 and 5
runs. On `latent.c`, whose lost update spans calls:

```
it fails without a switch at any load or store; trying branches and calls
that run switched at 1 hooked branches and calls
1 of 1 sites are needed (5 runs):
  latent 0x1000007f8 branch think (in latent) (latent.c:26)
```

That is where the thread was switched out, inside the function called
between the read and the write. For a branch or call suspect the racy
access is near the caller, not at the reported line.

## Tests (`tests/suspects_tests.rs`)

- The table has one entry per hooked instruction, and its loads and stores
  are the rewriter's memory sites.
- A mask of sites the run never switched at leaves the schedule hash
  unchanged; a mask of every load and store leaves no switch at one, and
  `lost_update` then passes.
- `suspects` on the first failing seed: every suspect is a load or store on
  the `// RACE` line, none on a `// DECOY` line. 8 of 8 repetitions.
- The branch-and-call phase on `latent.c`.

## Found on the way: a lock held by a dead process

The full suite hung once: the launcher spinning on the scheduler lock at
the end of a run, every guest already dead. At the end of a run with
daemons the launcher hands the baton on and then kills what is left; a
daemon killed inside a critical section took the lock with it. (Recorded
as a limit since the run-file review.) The lock word now holds its
owner's pid, and a waiter that has spun for a while takes the lock over if
the owner no longer exists, or is the waiter's own dead, unreaped child.
Unit test with a forked child that dies holding the lock, reaped and not.
400 runs of a daemon scenario under load: no hang.

## Limits

- The result is 1-minimal for this seed: no single site can be dropped.
  Another failing seed may need other sites; run it on a few.
- A masked run is one deterministic run. A site whose masking shifts later
  switches enough to lose the failure for unrelated reasons is kept as
  needed. Combining with seed bisection's probabilities would tell.
- Switches at blocking calls and at interposed system calls cannot be
  masked; only stubs can.
- ~~A process is matched to its program through its run-file entry, so
  children a guest spawns are not minimised over.~~ Every process
  announces its image to the launcher once it holds the baton, and the
  report says what each ran (`p<i>.program`, `p<i>.image`); `suspects`
  takes its programs from there (review of PRs #4 to #10, third pass).
- Needs `atos` (Xcode tools) for source lines; without it, addresses.
