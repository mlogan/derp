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
## 3. Real programs ✅

Homebrew's `curl` 8.18 fetches a page from each of two
`python3.13 -m http.server` instances, three virtual hosts, each server
serving its own `site/` copied into its own host directory. The client
appears as `10.0.0.3` in the servers' access logs, dated on the virtual
clock; 2 connections and 590 bytes through the virtual network, none
through the kernel; 7 threads. `repeat`: 20 identical runs at seed 1, 10
each at seeds 2-4, a different schedule hash per seed. Test:
`curl_fetches_pages_from_python_web_servers_on_two_hosts` (skips itself if
Homebrew's curl or python@3.13 are missing).

What these programs needed that the test programs never did:

- **`daemon: true`** in the run file. A server never exits; when every
  other initial process has, the launcher kills what is left. The process
  that just exited held the baton, so everything else is parked: the point
  is deterministic. Daemons do not count toward the run's exit status.
- **A cache directory for installed programs.** Rewritten copies of
  programs under `/opt`, `/usr`, `/Applications`, ... go to
  `$TMPDIR/rewrite-cache/<hash of path>/`; we do not write into the Cellar.
- **Threads the scheduler does not run.** GCD workers are made by the
  kernel, not `pthread_create`. curl's startup reads proxy settings through
  CoreFoundation and `dispatch_apply`, and hung. Three changes:
  wakes go to the scheduler *and* the kernel, from any thread; an
  unfair-lock or `dispatch_once` wait whose owner (its Mach port is in the
  lock word) is such a thread is a real wait, not yield-and-retry, which
  also burned virtual time at a real-time rate; and an idle scheduler looks
  again in real time, without moving the clock, for up to 30 s before it
  calls a deadlock when such threads exist. What those threads do is input.
- **`POSIX_SPAWN_SETEXEC` is an exec.** Python's launcher stub becomes the
  real interpreter in `Python.app` that way; treated as a child spawn, the
  old process record kept the baton forever.
- **Virtual pids are for guest code only.** libSystem's unified logging
  hands `getpid()` to `proc_pidinfo` while CoreFoundation initializes and
  crashes (SIGTRAP) on an error. Virtual pids 1000 and 1001 happened to be
  real processes on this machine, so only the third guest died. `getpid`
  and `getppid` now look at their return address: callers inside the dyld
  shared cache get the real pid.
- **Host directories, refined.** Metadata calls may name the directories
  above any permitted location (`realpath` reads the `/var` symlink), and
  outside they answer `ENOENT`, logged only when the path really exists:
  Python probes for many paths that are nowhere, which is not a
  misconfiguration. `~/.CFUserTextEncoding` is let through silently.
  curl's attempt to read `~/.curlrc` from the real home is refused and
  logged, which is the guard working.
- Socket options `TCP_KEEPINTVL`, `TCP_KEEPCNT`, `IPV6_V6ONLY` accepted.

Not needed, as it turned out: `EINPROGRESS` for non-blocking `connect`
(curl copes with immediate success), and IPv6 (Python was told
`--bind 0.0.0.0`; without it `http.server` asks the real resolver for a
wildcard address and may bind a kernel IPv6 socket).

## 4. Virtual pids from 100,000; GCD in guests is refused ✅ (2026-09-19)

- **`VPID_BASE` is 100,000.** Real pids on macOS never exceed 99,999, so a
  pid at or above the base is ours and anything below is the kernel's,
  whoever asks. `kill` and the `wait` family pass real pids through to the
  real calls (before, any pid in 1000..1255 was taken for a guest, a range
  real processes live in). A virtual pid that leaks into an untranslated
  kernel call now fails with "no such process" instead of naming a
  stranger. `getpid`/`getppid` still need the caller test. Not closed: a
  system API that returns the real pid to the guest
  (`NSProcessInfo.processIdentifier`).
- **`gcd.rs`: a guest that submits work to GCD exits with status 69** and
  `dispatch_async_f: Grand Central Dispatch is not supported; its worker
  threads run outside the scheduler`. Covered: `dispatch_async(_f)`,
  `dispatch_after(_f)`, `dispatch_apply(_f)`, `dispatch_group_async(_f)`,
  `dispatch_group_notify(_f)`, `dispatch_barrier_async(_f)`,
  `dispatch_source_create`, `dispatch_main`, `dispatch_read`,
  `dispatch_write`, `dispatch_io_create`. Only the guest's own code is
  held to it (caller outside the dyld shared cache): system libraries use
  GCD internally all the time, and refusing that would refuse curl.
  `dispatch_sync` and the semaphore calls stay available. Not covered:
  Swift concurrency, whose runtime is a system library.
- Each entry point has an assembly shim that saves the argument registers,
  hands the return address to the check, and branches into the real
  function. The interposer table passed 128 entries, so its length is now
  counted without macro recursion.

Found while re-running the curl/Python test next to the others (1 run in
15 to 30 differed, only under load):

- **The guest's stack moved by 16 bytes between runs.** The kernel puts the
  environment at the top of the stack, and two variables of ours had a
  varying length: the shared-state path (launcher pid) and the list of the
  launcher's own pipes (inode numbers). Both are fixed-width now. A switch
  site that was a stack address made this visible in the hash; the
  schedule itself was the same. The user's own environment has the same
  effect if it differs between runs.
- **Threads outside the scheduler no longer use the deterministic heap**
  (their allocations interleave in real time) and **no longer advance the
  virtual clock** when they read it (one run in sixty was one tick behind
  from the first switch on).
- `REWRITE_TRACE=file` writes one line per switch (from, to, issued, site,
  clock); diffing two traces is how both of the above were found.
  `p<i>.outside_wakes` counts scheduled threads woken by outside threads.
- After these: 80 of 80 traces byte-identical under 8 busy-loop processes.

## 5. Guests start from a fixed environment ✅ (2026-09-19)

- Run-file runs no longer inherit the launcher's environment. Every guest
  starts from `PATH=/usr/bin:/bin:/usr/sbin:/sbin`, `LANG=C`, `LC_ALL=C`,
  `TZ=UTC`, `USER=guest`, `LOGNAME=guest`; then what the run file names in
  `pass-env:` (taken from the launcher's environment), the host's `HOME`,
  `PWD` and `TMPDIR`, the run file's top-level `env:`, the process's own
  `env:`, and the supervisor's variables. Each name appears once.
- Why: the shell's variables were an unrecorded input (`http_proxy` would
  send curl out of the virtual network; `LANG` and `TZ` change output), and
  their total length decides where the guest's stack starts.
- Checked first: the strings the kernel puts above the environment
  (`apple[]`) were a constant 292 bytes over 60 launches, so the environment
  was the only variable part.
- The default scratch directory's name is fixed width too (it is in `HOME`,
  `PWD` and `TMPDIR`). The launcher forwards `REWRITE_TRACE` and
  `REWRITE_PARK_SPINS` explicitly. A trace path of another length moves the
  stack, so compare traces written to paths of equal length.
- Single-program `rewrite run prog` still inherits the environment: that is
  the quick way to try a binary, and people expect their variables there.
- Test: `envprobe.c` under two ambient environments that differ by 3 KB, a
  proxy, a locale and a time zone. Same variable names, same values, same
  stack address, byte-identical trace. The test fails if inheritance is
  switched back on (checked).

## 6. Review (2026-09-19)

Five read-only reviewers (scheduler core, virtual network, interposers,
launcher and rewriter, simplifications). Findings were checked against the
code, and two by experiment, before anything was changed.

Fixed, with regression tests where marked (T):
- Scheduler: no baton holder after a process died over a deadlock (T); a
  wake from an outside thread lost between the block check and the block;
  `kill` then `waitpid` a false deadlock (T); SIGTERM handlers would run
  off the baton (delivered as SIGKILL now); exiting threads and processes
  without outside threads aborted instead of waiting for an outside wake;
  signals to `exec`-retired thread records; leaked thread port rights;
  timeout overflow; 1,024-thread and 256-process caps raised to 16,384 and
  1,024 (a thread-per-request server reached the old one quickly).
- Virtual network: payloads in flight to a reused slot stayed counted and
  the idle clock jumped to `u64::MAX` (T); `shutdown` then `close`
  overflowed the flight ring with a second FIN (T); datagrams ignored the
  bound address (T); `readv`/`writev` split datagrams; missed `EV_CLEAR`
  write edges; kqueue registry after `fork` and `dup2`; UNIX datagram
  connects to system sockets; `select` on a bad descriptor; accepted
  sockets and `O_NONBLOCK`; references after an `execve` that closed all.
- Interposers: **the path guard did nothing under the default scratch
  directory** (it is below `/var/folders`, a system location) (T); paths
  relative to a directory descriptor; eight more guarded calls; a child's
  real pid of 0 made `wait4` and `kill` group-wide; `vfork`; spawns from
  outside threads inherited `REWRITE_PROC`; outside threads drew from the
  seeded entropy stream, and fork children shared their parent's; private
  spinlocks held across `fork`; fd 240 closed by close-everything loops;
  kernel sockets and the fixed-width external list; `flock` unlock with
  `LOCK_NB`; allocator size overflow.
- `dispatch_semaphore_wait` deadlines (T): libdispatch reads the clock
  through our interposers, so a 2 s timeout expired after 1 ms of virtual
  time. Found by running a guest, as the reviewer suggested.
- Launcher: daemons never ended a `--native` run (T); a failed `waitpid`
  hung it; early deaths reported to the scheduler twice; busy loop at
  socket EOF; cache temporary name and key; shared-state file under `/tmp`.
- Run file: unknown keys in a process map (T), reserved variable names (T),
  host names that collide with `stdout.<n>` or differ only in case (T).

Refuted by experiment: that `malloc_type_*` entry points bypass the
interposed allocator. `getline` grows a guest buffer inside the
deterministic heap, and C++ `new` lands there too.

Simplified: the GCD shims (387 lines to 84, one macro), one descriptor
classifier in `io.rs`, one `errno` helper, one I/O wait and wake, dead
fields and the launcher's placement check, `REWRITE_HOSTS` and the
always-empty `env` fields, `Death` and the overloaded `Handoff::Stay`.

Recorded, not fixed:
- A `poll` or `kevent` that mixes virtual sockets with descriptors whose
  peer is outside the run is not woken by the outside ones.
- A process that dies holding the shared spinlock (an outside thread
  killed mid-wake) wedges the run. Low probability, no recovery.
- Per-process hook counts are wrong when an outside thread expires the
  quantum, and across `execve` (report only).
- Wake keys are per process: process-shared mutexes and condition variables
  across `fork` never wake. `pthread_rwlock`, `sem_wait` and
  `pthread_cond_timedwait_relative_np` are not interposed.
- An outside thread freeing a block of the deterministic heap perturbs its
  free lists at a real-time moment.
- Unguarded path calls that remain: `getattrlist`, `getxattr`/`setxattr`,
  `chflags`, `renamex_np`, `fclonefileat`, `shm_open`/`sem_open` names.
- A reply left in the launcher socket by a guest killed mid-request would
  desynchronize later requests.
- The rewritten executable's cache path (under `$TMPDIR`) is on the guest's
  stack; a different `TMPDIR` moves stack addresses.

Simplifications left for later (each is mechanical but touches many lines):
one vocabulary (`proc`/`vpid`/`real_pid`; "run file" instead of "manifest"
in code; "scheduled" and "outside" threads) and one guard idiom; `Launch`
folded into `Run`; a `Ring` type for the two rings in `netstate.rs`; the
test boilerplate in `multiproc_tests.rs`; moving the pthread emulation out
of `interpose.rs`, the descriptor calls out of `net.rs`, and the run-file
policy out of `main.rs`; splitting `supervise`, `my_kevent` and
`yield_baton_as`.

## Test summary

`cargo test -p rewrite -p rewrite-supervisor`: all pass (28 + 4 + 24 + 3 + 4
in `rewrite`, 23 unit tests in the supervisor); clippy clean.

## Follow-ups

- Work done by GCD worker threads is outside the schedule. For curl that is
  one preferences lookup at startup; a program that does real work on
  dispatch queues would not be deterministic. Interposing
  `dispatch_async` and friends to run blocks on scheduled threads is the
  way in.
- `allow:` is for the whole run; a per-host list would be easy.
