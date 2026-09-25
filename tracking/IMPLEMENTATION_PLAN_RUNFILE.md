# Run Files, Host Directories, Real Programs

Follow-up to `IMPLEMENTATION_PLAN_MULTIPROC.md`, on the same branch
(`mlogan-multiproc`). Three steps, each feeding the next.

## 1. The run file is YAML

Replaces the ad-hoc manifest format. `derp run|repeat --manifest run.yaml`.

```yaml
seed: 7                  # optional; the command line overrides all four
quantum: 1000..10000
mem-hook-rate: 1/16
net-latency: 5ms
allow:                   # extra paths every host may touch (step 2)
  - /opt/site-content
hosts:                   # a list: order assigns 10.0.0.1, 10.0.0.2, ...
  - name: alpha
    files: [site/index.html, site/img]   # copied into the host's fresh directory (step 2)
    processes:
      - [server, --port, 8080]          # argv verbatim
      - client alpha 8080               # or a line, split on whitespace
      - argv: [worker, "two words"]     # or a map, with an environment
        env: { MODE: fast }
```

- Parsed with `serde_yaml` into typed structs; unknown keys are errors.
- Program paths stay relative to the run file; `argv[0]` stays the token.
- The old format and its parser go away; tests are converted.

## 2. A directory per host

Not a sandbox: a guard against configurations that would let one host see
another's files by accident. Symlinks and deliberate tricks may escape.

- The launcher creates every host's root directory fresh at the start of
  each run, as `<scratch>/<name>`. The run file never names it. (Mark,
  2026-09-19: "we don't want to declare a host dir, we want the supervisor
  to create one for us each time we start.")
- A host's `files:` are inputs copied into the fresh root before anything
  starts: a file lands as `<root>/<basename>`, a directory as
  `<root>/<basename>/` with its contents. Paths are relative to the run file.
- Processes start with the root as cwd, `PWD`, `HOME`, and `TMPDIR=<root>/tmp`.
  Spawned children inherit the host and therefore the root.
- The supervisor interposes the calls that take path names. A path is made
  absolute against the cwd and normalized lexically (`.` and `..`, no
  symlink resolution), then must be:
  - inside the host's root, or
  - under a system location every program reads through libSystem
    (`/usr`, `/System`, `/Library`, `/bin`, `/sbin`, `/dev`, `/etc`,
    `/var/db`, `/opt/homebrew`, `/usr/local`, with the `/private` aliases), or
  - under an `allow:` entry of the run file.
  Anything else fails with `EACCES` and is logged with the host, the call
  and the path.
- Program images are exempt: `execve`/`posix_spawn` paths are not checked
  (guests' binaries live outside every host's root).
- Calls: `open`, `openat`, `creat`, `stat`, `lstat`, `access`, `mkdir`,
  `rmdir`, `unlink`, `rename`, `link`, `symlink`, `readlink`, `chdir`,
  `truncate`, `chmod`, `chown`, `utimes`, `mkfifo`, `opendir`, `realpath`,
  and the `$NOCANCEL` forms libSystem uses. UNIX-domain socket paths are
  already per-host names with no filesystem node.
- Single-program `derp run prog` has no host root and no checks.
- Replaces the "first path outside the scratch directory" log.

Tests: a C program that reads and writes inside its root, is refused another
host's file by absolute path and by `../other`, may read `/etc/hosts`, and
whose spawned child is held to the same root; two hosts writing the same
relative path end up with different files.

## 3. Real programs

Homebrew's `curl` fetching pages from `python3 -m http.server`, each on its
own host with its own directory, under `repeat`. Apple's own binaries ignore
`DYLD_INSERT_LIBRARIES`, so the guests are Homebrew's. Whatever these
programs need that the supervisor lacks is the real output of this step;
gaps are fixed or recorded.

## Acceptance

- Every existing multi-process test passes from YAML run files.
- Bad run files fail with a message naming the problem.
- The host-directory tests above pass; violations are logged.
- `curl` fetches a page from the Python server by virtual host name, the
  page differs per host, and 20 runs per seed are identical. Anything that
  stops short of this is written down with its cause.
