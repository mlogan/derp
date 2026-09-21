# Sui under the supervisor: progress

Goal: run `sui start` (a local network: validators and a fullnode in one
process) deterministically under `rewrite run`, end the run after a fixed
virtual time, and compare `RUST_LOG=trace` output between runs.

Branch `mlogan-sui`. Sui checkout: `~/repos/sui` (main), built with
`cargo build --release --bin sui` (the `release` profile: `panic=abort`,
line tables only, a separate dSYM).

## Findings so far (2026-09-21)

1. **The debug `sui` cannot be rewritten**: its `__text` is 291 MB, and a
   hooked site reaches its stub with one `b` (±128 MB). No placement of a
   stub segment covers it. Release only.
2. **Stubs are 60 to 68 bytes per site.** Millions of sites would put the
   stub segment itself out of reach. Fix: one shared body, and a 16 to 24
   byte trampoline per site (below).
3. `sui start` never exits, so a run needs a stop at a virtual time:
   `stop-after: 30s` (run file) or `--stop-after 30s`.

## Done

- (in progress) `stop-after`: the scheduler ends the run when the virtual
  clock reaches the time; every process is killed at that point of the
  schedule, reported as `stopped`, and does not fail the run.

## Remaining

- Shared stub body; per-site trampolines.
- Run `sui start --force-regenesis` under a run file with `RUST_LOG=trace`,
  `RUST_LOG_FILE` in the host directory; compare logs between runs.
- Whatever that finds.
