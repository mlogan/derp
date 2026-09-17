# Rewrite Experiment: Progress

Tracks `IMPLEMENTATION_PLAN_REWRITE.md`. Updated at the end of each work
unit.

## Layout (differs from the plan)

- the repository — crate `rewrite`: `macho.rs` (reader/writer), `launch.rs`
  (posix_spawn launcher), `rng.rs`, CLI in `main.rs`.
- `supervisor/` — crate `rewrite-supervisor`, the injected cdylib.
  It is a separate crate because the interposers and constructor must not
  be linked into the launcher binary.
- Guests must be linked with `-Wl,-headerpad,0x1000`: the rewriter adds two
  segment load commands (304 bytes) and default binaries have no room.

## Day 1 — Mach-O reader/writer and launcher ✅

- Parse header, load commands, segments, sections, `LC_FUNCTION_STARTS`,
  `LC_DATA_IN_CODE`.
- `emit()` inserts `__STUBD` (rw, one page) and `__STUB` (rx) in front of
  `__LINKEDIT`, shifts every linkedit file offset, drops the old
  `LC_CODE_SIGNATURE`; `adhoc_sign()` shells out to `codesign -s -`.
- Launcher spawns with `_POSIX_SPAWN_DISABLE_ASLR`, injects the dylib,
  reads a `key=value` report from an inherited pipe.
- Hello world round-trips and runs standalone and supervised
  (`tests/macho_tests.rs`).

## Day 2 — Branch rewriting, stubs, overhead ⏳

## Day 3 — Threads, baton scheduler, blocking primitives

## Day 4 — Seeded quanta, trace, memory hooks

## Day 5 — Determinism hardening, measurements, write-up
