# Multi-Process Deterministic Runs: Progress

Tracks `IMPLEMENTATION_PLAN_MULTIPROC.md`. Branch `mlogan-multiproc`
(branched from `mlogan-rewrite`, which is `origin/main` plus the plan).
Updated at the end of each work unit.

## Day 1 — Shared scheduler, cross-process baton, manifest (in progress)

- [ ] `shared.rs`: `#[repr(C)]` state, spinlock, park words, pick/yield
- [ ] Supervisor scheduler and interposers moved onto the shared state
- [ ] Launcher creates the shared file, registers initial processes, hands
      out the first baton, reaps and hands the baton on when a guest dies
- [ ] Manifest parsing and `rewrite run --manifest`
- [ ] `mutex.c`, `race.c`, `channel.rs` still pass
- [ ] Two `loops` processes alternate with a stable run-wide hash

## Days 2-12 — not started

See the plan's implementation order.

## Notes
