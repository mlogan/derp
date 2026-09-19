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
## 2. Host directories ✅

- `src/hostdir.rs`: the launcher creates `<scratch>/<host>` (with a
  `tmp` inside) fresh for every run; the run file never names it. A host's
  `files:` are copied in first (directories recursively, in sorted order).
- Guests start there with `PWD`, `HOME` and `TMPDIR` inside it. Found on the
  way: variables the run sets must *replace* inherited ones, because
  `getenv` returns the first match.
- `supervisor/src/hostfs.rs`: a path is made absolute against the
  cwd, normalized lexically, and must be under the host's directory, a
  system location, or an `allow:` entry; otherwise `EACCES` and a log line
  with host, call and path (first 20 per process; `p<i>.paths_refused`
  counts all). Symlinks are not resolved, by design.
- Calls that only ask about a path (`stat`, `lstat`, `fstatat`, `access`,
  `readlink`) may also name the directories *above* the host's: programs
  walk them (`realpath`, `mkdir -p`) and they reveal nothing of another
  host. Opening or listing them is refused.
- Guarded: `open`, `open$NOCANCEL`, `openat`, `creat`, `stat`, `lstat`,
  `fstatat`, `access`, `mkdir`, `mkdirat`, `rmdir`, `unlink`, `unlinkat`,
  `rename`, `link`, `symlink` (where the link is made), `readlink`,
  `chdir`, `truncate`, `chmod`, `chown`, `utimes`, `mkfifo`, `opendir`.
  Paths relative to a directory descriptor were checked when it was opened.
- Not guarded: program images (`execve`, `posix_spawn`), and the
  supervisor's own lookups when it resolves one.
- System locations: `/usr`, `/System`, `/Library`, `/bin`, `/sbin`, `/dev`,
  `/etc`, `/var/db`, `/var/run`, `/var/folders` (libSystem's per-user
  caches), `/opt/homebrew`, `/Applications/Xcode*`, with `/private` aliases.
- Gotcha: libSystem's `getcwd` opens `"."` itself, which re-enters the
  interposers. The guard takes the cwd before its lock and marks the thread
  so the inner call passes; the first version spun on its own lock.
- Spawned children inherit the policy even if the guest passes its own
  `envp`. Single-program `rewrite run prog` has no policy and pays nothing.
- The "first path outside the scratch directory" log is gone.
- Test: `hostfs.c` on two hosts, 23 lines of allowed and refused operations
  each, the same relative name being a different file per host, `files:`
  copied to one host only, a stale file from an earlier run gone.
## 3. Real programs (not started)
