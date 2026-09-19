# Run Files, Host Directories, Real Programs: Progress

Tracks `IMPLEMENTATION_PLAN_RUNFILE.md`. Branch `mlogan-multiproc`.

## 1. YAML run file ✅

- `src/manifest.rs` parses the run file with `serde_yaml` into typed
  structs (`deny_unknown_fields`, so a typo is an error that names the key).
  New dependencies of the `rewrite` crate: `serde`, `serde_yaml` 0.9 (it is
  archived upstream but stable; it was already in the local cargo cache).
- Hosts are a list, because their order assigns addresses. A process is a
  line (split on whitespace), a list (argv verbatim) or a map with `argv`
  and `env`.
- `seed`, `quantum`, `mem-hook-rate` and `net-latency` may live in the run
  file; a flag given on the command line wins.
- The ad-hoc format and its tokenizer are gone. All 16 multi-process tests
  were converted and pass; a new test covers settings, the override, the
  per-process environment and error messages (including that a `root:` key
  is rejected: host directories are never declared).
## 2. Host directories (not started)
## 3. Real programs (not started)
