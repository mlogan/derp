use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use rewrite::cache::{cached_rewrite, read_macho, rewrite_file, write_exe, Fallible};
use rewrite::launch::{self, Guest, Launch, Run, RunOutcome};
use rewrite::manifest::{self, parse_duration_ns};
use rewrite::rewrite::{self as rw, Options};

const USAGE: &str = "\
usage:
  derp copy <in> <out>                 round-trip a binary through the writer and re-sign it
  derp scan [opts] <prog>              print what the rewriter would hook
  derp rewrite [opts] <in> <out>       rewrite and sign
  derp run [opts] <prog> [args…]       rewrite (cached), then launch under the supervisor
  derp bench [opts] <prog> [args…]     time native vs rewritten (no supervisor)
  derp repeat [opts] <prog> [args…]    run N times; exit status, stdout and schedule hash must agree
  derp bisect [opts] --manifest FILE   when was the failing seed's failure decided? Replays it
                                       with every stream reseeded at a virtual time, --runs
                                       futures per probe (default 20), --jobs at a time (4),
                                       down to --resolution (2ms)
  derp suspects [opts] --manifest FILE  which loads and stores does the failing seed need?
                                       Masks switch points at hooked loads and stores until
                                       none can be dropped, and names their source lines
  derp run|repeat [opts] --manifest FILE
                                       several processes under one scheduler; see below
  derp cargo <cargo args…>             cargo, with `derp cc` as the linker: a program too big
                                       for its sites to reach the stubs is linked again with
                                       rooms for them in its text (README, Big programs)
  derp cc <linker args…>               the linker driver `derp cargo` installs
  derp rooms <prog>                    the rooms `derp cc` would give <prog>, and why
options:
  --runs N                             repetitions for repeat (default 100)
  --seed S                             run seed (default 0)
  --mem-hook-rate R                    0, 1 or a fraction like 1/16 (default 0)
  --quantum LO..HI                     hook events per quantum (default 1000..10000)
  --reseed-at T --reseed N             from virtual time T on, every random stream (schedule,
                                       faults, heap layout, entropy) starts over from N: what
                                       bisect does at each probe
  --heap-size N                        address space of each guest's heap: 32G (default), 512M,
                                       4T. Only touched pages cost memory
  --jobs J                             bisect, suspects: runs at a time (default 4)
  --resolution T                       bisect: stop at an interval this short (default 2ms)
  --no-supervisor                      no scheduling: the dylib only provides the stubs' counter
  --aslr                               leave ASLR on
  --native                             run the original binary without the dylib
  --manifest FILE                      run file (YAML): hosts and their processes
  --scratch DIR                        where a run file's host directories are made, fresh
                                       (default: a directory under the system temp dir)
  --capture                            manifest run: each guest's stdout goes to stdout.<index>
                                       in the scratch directory instead of ours
  --capture-stderr                     and its stderr to stderr.<index>, with the supervisor's
                                       messages about it
  --net-latency T                      virtual-time delay between different hosts, such as
                                       5ms, 250us or 1s (default 0)
  --switch-cost T                      virtual time each baton hand-off costs (default 10us)
  --stop-after T                       the run is over at this virtual time: what still runs
                                       is killed there, reported as stopped, and does not
                                       fail the run (for servers that never exit)
  --wall-limit T                       the same at this real time, for native runs too: to
                                       compare the CPU time (cpu_user_ns, cpu_system_ns) a
                                       program uses natively and under the supervisor
run file:
  seed: 7                              optional; the command line overrides these
  quantum: 1000..10000
  mem-hook-rate: 1/16
  net-latency: 5ms
  switch-cost: 10us
  heap-size: 32G
  stop-after: 30s
  wall-limit: 60s
  env: { LOG_LEVEL: debug }            for every process
  pass-env: [SSL_CERT_FILE]            inherited from your environment; nothing else is
  hosts:                               in order: 10.0.0.1, 10.0.0.2, ...
    - name: alpha
      files: [site/index.html]         copied into the host's fresh directory
      processes:
        - [server, --port, 8080]       argv verbatim
        - client alpha 8080            or a line, split on whitespace
        - argv: [worker]               or a map, with an environment
          env: { MODE: fast }
  Program paths are relative to the run file. See README.md.
";

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("derp: {msg}");
    ExitCode::from(2)
}

#[derive(Clone)]
struct Cli {
    /// Settings given on the command line, which a run file cannot override
    given: Vec<&'static str>,
    opts: Options,
    quantum: (u32, u32),
    supervisor: bool,
    manifest: Option<PathBuf>,
    scratch: Option<PathBuf>,
    capture: bool,
    /// Each guest's stderr to a file too; otherwise it stays ours, where
    /// the supervisor's own messages about a guest are expected
    capture_stderr: bool,
    net_latency_ns: u64,
    switch_ns: u64,
    heap_size: u64,
    /// Virtual time at which the run is over (0: when its processes are)
    stop_after_ns: u64,
    /// Real time after which it is over (0: never)
    wall_limit_ms: u64,
    reseed_at: Option<u64>,
    reseed: u64,
    jobs: u32,
    resolution_ns: u64,
    disable_aslr: bool,
    native: bool,
    runs: u32,
    rest: Vec<OsString>,
}

fn parse_quantum(v: &str) -> Result<(u32, u32), String> {
    v.split_once("..")
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
        .filter(|&(lo, hi): &(u32, u32)| lo >= 1 && hi >= lo)
        .ok_or(format!("bad quantum {v}"))
}

/// At least a microsecond: a lone thread that computes without reading the
/// clock moves it only by its hand-offs, and a sleeper's deadline must pass.
fn parse_switch_cost(v: &str) -> Result<u64, String> {
    parse_duration_ns(v)
        .filter(|&ns| ns >= 1_000)
        .ok_or(format!("bad switch cost {v} (at least 1us)"))
}

/// The run's settings: the run file's, unless the command line gave them.
fn with_run_file_settings(cli: &Cli, m: &manifest::Manifest) -> Result<Cli, String> {
    let mut cli = cli.clone();
    let from_file = |name: &str| !cli.given.contains(&name);
    if let (Some(seed), true) = (m.seed, from_file("seed")) {
        cli.opts.seed = seed;
    }
    if let (Some(q), true) = (&m.quantum, from_file("quantum")) {
        cli.quantum = parse_quantum(q)?;
    }
    if let (Some(r), true) = (&m.mem_hook_rate, from_file("mem-hook-rate")) {
        cli.opts.mem_rate = rw::parse_rate(r).ok_or(format!("bad rate {r}"))?;
    }
    if let (Some(l), true) = (&m.net_latency, from_file("net-latency")) {
        cli.net_latency_ns = parse_duration_ns(l).ok_or(format!("bad duration {l}"))?;
    }
    if let (Some(c), true) = (&m.switch_cost, from_file("switch-cost")) {
        cli.switch_ns = parse_switch_cost(c)?;
    }
    if let (Some(h), true) = (&m.heap_size, from_file("heap-size")) {
        cli.heap_size = manifest::parse_size(h).ok_or(format!("bad heap size {h}"))?;
    }
    if let (Some(t), true) = (&m.stop_after, from_file("stop-after")) {
        cli.stop_after_ns = parse_stop_after(t)?;
    }
    if let (Some(t), true) = (&m.wall_limit, from_file("wall-limit")) {
        cli.wall_limit_ms = parse_stop_after(t)? / 1_000_000;
    }
    Ok(cli)
}

fn parse_stop_after(v: &str) -> Result<u64, String> {
    match parse_duration_ns(v) {
        Some(0) | None => Err(format!("bad stop-after time {v}")),
        Some(ns) => Ok(ns),
    }
}

fn parse_cli(mut args: Vec<OsString>) -> Result<Cli, String> {
    let mut cli = Cli {
        given: Vec::new(),
        opts: Options::default(),
        quantum: launch::DEFAULT_QUANTUM,
        supervisor: true,
        manifest: None,
        scratch: None,
        capture: false,
        capture_stderr: false,
        net_latency_ns: 0,
        switch_ns: rewrite::shared::DEFAULT_SWITCH_NS,
        heap_size: launch::DEFAULT_HEAP,
        stop_after_ns: 0,
        wall_limit_ms: 0,
        reseed_at: None,
        reseed: 0,
        jobs: 4,
        resolution_ns: 2_000_000,
        disable_aslr: true,
        native: false,
        runs: 100,
        rest: Vec::new(),
    };
    while let Some(a) = args.first().and_then(|a| a.to_str()).map(str::to_owned) {
        let take_value = |args: &mut Vec<OsString>| -> Result<String, String> {
            if args.len() < 2 {
                return Err(format!("{a} needs a value"));
            }
            args.remove(0);
            Ok(args[0].to_string_lossy().into_owned())
        };
        match a.as_str() {
            "--seed" => {
                cli.opts.seed = take_value(&mut args)?.parse().map_err(|_| "bad seed")?;
                cli.given.push("seed");
            }
            "--mem-hook-rate" => {
                let v = take_value(&mut args)?;
                cli.opts.mem_rate = rw::parse_rate(&v).ok_or(format!("bad rate {v}"))?;
                cli.given.push("mem-hook-rate");
            }
            "--quantum" => {
                cli.quantum = parse_quantum(&take_value(&mut args)?)?;
                cli.given.push("quantum");
            }
            "--manifest" => cli.manifest = Some(take_value(&mut args)?.into()),
            "--scratch" => cli.scratch = Some(take_value(&mut args)?.into()),
            "--capture" => cli.capture = true,
            "--capture-stderr" => cli.capture_stderr = true,
            "--net-latency" => {
                let v = take_value(&mut args)?;
                cli.net_latency_ns = parse_duration_ns(&v).ok_or(format!("bad duration {v}"))?;
                cli.given.push("net-latency");
            }
            "--switch-cost" => {
                cli.switch_ns = parse_switch_cost(&take_value(&mut args)?)?;
                cli.given.push("switch-cost");
            }
            "--heap-size" => {
                let v = take_value(&mut args)?;
                cli.heap_size = manifest::parse_size(&v).ok_or(format!("bad heap size {v}"))?;
                cli.given.push("heap-size");
            }
            "--wall-limit" => {
                cli.wall_limit_ms = parse_stop_after(&take_value(&mut args)?)? / 1_000_000;
                cli.given.push("wall-limit");
            }
            "--stop-after" => {
                cli.stop_after_ns = parse_stop_after(&take_value(&mut args)?)?;
                cli.given.push("stop-after");
            }
            "--reseed-at" => {
                let v = take_value(&mut args)?;
                cli.reseed_at = Some(parse_duration_ns(&v).ok_or(format!("bad time {v}"))?);
            }
            "--jobs" => {
                cli.jobs = take_value(&mut args)?
                    .parse()
                    .map_err(|_| "bad job count")?;
            }
            "--resolution" => {
                let v = take_value(&mut args)?;
                cli.resolution_ns = parse_duration_ns(&v).ok_or(format!("bad duration {v}"))?;
            }
            "--reseed" => {
                cli.reseed = take_value(&mut args)?
                    .parse()
                    .map_err(|_| "bad reseed value")?;
                cli.given.push("reseed");
            }
            "--no-supervisor" => cli.supervisor = false,
            "--aslr" => cli.disable_aslr = false,
            "--native" => cli.native = true,
            "--runs" => {
                cli.runs = take_value(&mut args)?.parse().map_err(|_| "bad --runs")?;
                cli.given.push("runs");
            }
            _ if a.starts_with("--") => return Err(format!("unknown option {a}")),
            _ => break,
        }
        args.remove(0);
    }
    cli.rest = args;
    if cli.given.contains(&"reseed") && cli.reseed_at.is_none() {
        return Err("--reseed needs --reseed-at: from when?".into());
    }
    Ok(cli)
}

fn copy(input: &Path, output: &Path) -> Fallible<()> {
    write_exe(output, &read_macho(input)?.emit(&[], &[])?)
}

/// Every rewritten binary needs the dylib; only native runs go without.
fn dylib_for(cli: &Cli) -> Fallible<Option<PathBuf>> {
    if cli.native {
        return Ok(None);
    }
    launch::default_dylib()
        .map(Some)
        .ok_or_else(|| "supervisor dylib not found next to the derp binary".into())
}

fn run_guest(
    exe: PathBuf,
    cli: &Cli,
    args: Vec<OsString>,
    quiet: bool,
    stdout: Option<PathBuf>,
) -> Fallible<launch::Outcome> {
    let cfg = Launch {
        exe,
        args,
        dylib: dylib_for(cli)?,
        disable_aslr: cli.disable_aslr,
        heap_size: cli.heap_size,
        stdout,
        stderr: None,
        seed: cli.opts.seed,
        quantum: cli.quantum,
        stop_at_ns: cli.stop_after_ns,
        wall_limit_ms: cli.wall_limit_ms,
        passive: !cli.supervisor,
        rewrite: (!cli.native).then(|| cli.opts.clone()),
    };
    let outcome = launch::launch(&cfg)?;
    if !quiet {
        for (k, v) in &outcome.report.fields {
            eprintln!("{k}={v}");
        }
    }
    Ok(outcome)
}

const SCRATCH_MARKER: &str = ".rewrite-scratch";

/// Create the run's scratch directory empty. An existing directory is
/// only cleared if an earlier run of ours marked it.
fn prepare_scratch(dir: &Path) -> Fallible<()> {
    if dir.exists() {
        let ours = dir.join(SCRATCH_MARKER).exists();
        if !ours && std::fs::read_dir(dir)?.next().is_some() {
            return Err(format!(
                "{} exists, is not empty and was not created by derp",
                dir.display()
            )
            .into());
        }
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(SCRATCH_MARKER), "")?;
    Ok(())
}

/// Fixed width: guests' `HOME`, `PWD` and `TMPDIR` contain this path, and
/// the length of the environment decides where a guest's stack starts.
fn scratch_dir(cli: &Cli) -> PathBuf {
    cli.scratch.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("rewrite-run-{:010}", std::process::id()))
    })
}

/// What every guest of a run-file run starts from, instead of our own
/// environment. Anything else comes from the run file, by value (`env:`)
/// or by name (`pass-env:`).
const FIXED_ENV: [(&str, &str); 6] = [
    ("PATH", "/usr/bin:/bin:/usr/sbin:/sbin"),
    ("LANG", "C"),
    ("LC_ALL", "C"),
    ("TZ", "UTC"),
    ("USER", "guest"),
    ("LOGNAME", "guest"),
];

/// Later entries replace earlier ones, so every name appears once:
/// `getenv` returns the first match.
fn layered(layers: &[&[(String, String)]]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in layers.iter().flat_map(|l| l.iter()) {
        match out.iter_mut().find(|(name, _)| name == k) {
            Some(entry) => entry.1.clone_from(v),
            None => out.push((k.clone(), v.clone())),
        }
    }
    out
}

fn stdout_file(scratch: &Path, index: usize) -> PathBuf {
    scratch.join(format!("stdout.{index}"))
}

fn stderr_file(scratch: &Path, index: usize) -> PathBuf {
    scratch.join(format!("stderr.{index}"))
}

/// The run file's `allow:` entries as the supervisor compares them: as
/// text, against paths with `/private` taken off `/tmp`, `/var` and
/// `/etc`. A relative entry is next to the run file, like `argv[0]`, and
/// is resolved through symlinks, since guests name the target.
fn allowed_paths(base: &Path, allow: &[String]) -> Fallible<Vec<String>> {
    allow
        .iter()
        .map(|entry| {
            if entry.starts_with('/') {
                return Ok(entry.clone());
            }
            let path = std::fs::canonicalize(base.join(entry))
                .map_err(|e| format!("allow: {entry}: {e}"))?;
            let text = path.to_string_lossy().into_owned();
            let public = text.strip_prefix("/private").filter(|rest| {
                ["/tmp", "/var", "/etc"].iter().any(|d| {
                    rest.strip_prefix(d)
                        .is_some_and(|r| r.is_empty() || r.starts_with('/'))
                })
            });
            Ok(public.map_or(text.clone(), str::to_string))
        })
        .collect()
}

/// Start the manifest's processes under one scheduler. With `capture`,
/// each guest's stdout goes to `stdout.<index>` in the scratch directory.
fn run_manifest(cli: &Cli, path: &Path, scratch: &Path, capture: bool) -> Fallible<RunOutcome> {
    let m = manifest::parse(&std::fs::read_to_string(path)?)?;
    let cli = &with_run_file_settings(cli, &m)?;
    let base = path.parent().unwrap_or(Path::new("."));
    // Programs first: a run file that names a missing one should fail
    // before anything is created.
    let mut programs = Vec::new();
    for p in &m.processes {
        let prog = std::fs::canonicalize(base.join(&p.argv[0]))
            .map_err(|e| format!("{}: {e}", p.argv[0]))?;
        programs.push(if cli.native {
            prog
        } else {
            cached_rewrite(&prog, &cli.opts)?
        });
    }
    let allow = allowed_paths(base, &m.allow)?;
    prepare_scratch(scratch)?;
    let scratch = std::fs::canonicalize(scratch)?;
    let roots = rewrite::hostdir::prepare(&scratch, base, &m.hosts)?;
    let text = |p: &Path| p.to_string_lossy().into_owned();
    let fixed: Vec<(String, String)> = FIXED_ENV
        .iter()
        .map(|&(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let passed: Vec<(String, String)> = m
        .pass_env
        .iter()
        .filter_map(|k| Some((k.clone(), std::env::var(k).ok()?)))
        .collect();
    let guests = m
        .processes
        .iter()
        .zip(programs)
        .enumerate()
        .map(|(i, (p, exe))| {
            let root = &roots[p.host as usize];
            // The host's directory is home: where the process starts and
            // what its path names are held to
            let host_env = vec![
                ("PWD".to_string(), text(root)),
                ("HOME".to_string(), text(root)),
                ("TMPDIR".to_string(), text(&root.join("tmp"))),
            ];
            let policy = vec![
                (rewrite::shared::HOST_ROOT_VAR.to_string(), text(root)),
                (rewrite::shared::ALLOW_VAR.to_string(), allow.join(":")),
            ];
            let env = layered(&[&fixed, &passed, &host_env, &m.env, &p.env, &policy]);
            Guest {
                exe,
                argv0: Some(p.argv[0].clone().into()),
                args: p.argv[1..].iter().map(Into::into).collect(),
                host: p.host,
                env,
                stdout: capture.then(|| stdout_file(&scratch, i)),
                stderr: cli.capture_stderr.then(|| stderr_file(&scratch, i)),
                cwd: Some(root.clone()),
                daemon: p.daemon,
                faults: p.faults,
            }
        })
        .collect();
    let run = Run {
        guests,
        hosts: m.hosts.iter().map(|h| h.name.clone()).collect(),
        dylib: dylib_for(cli)?,
        inherit_env: false,
        disable_aslr: cli.disable_aslr,
        heap_size: cli.heap_size,
        seed: cli.opts.seed,
        quantum: cli.quantum,
        passive: !cli.supervisor,
        rewrite: (!cli.native).then(|| cli.opts.clone()),
        net_latency_ns: cli.net_latency_ns,
        switch_ns: cli.switch_ns,
        reseed: cli.reseed_at.map(|at| (at, cli.reseed)),
        stop_at_ns: cli.stop_after_ns,
        wall_limit_ms: cli.wall_limit_ms,
        outside_network: m.outside_network,
    };
    if run.stop_at_ns != 0 && (run.passive || run.dylib.is_none()) {
        return Err("stop-after needs the supervisor: it is a virtual time".into());
    }
    let has_faults = run
        .guests
        .iter()
        .any(|g| g.faults.restart != rewrite::shared::RESTART_NEVER || g.faults.crash_hi_ns != 0);
    if has_faults && (run.passive || run.dylib.is_none()) {
        return Err("crash and restart settings need the supervisor".into());
    }
    let outcome = launch::launch_run(&run)?;
    if outcome.totals.restarts_refused > 0 {
        return Err(format!(
            "the run's process table is full: {} restarts were not made",
            outcome.totals.restarts_refused
        )
        .into());
    }
    if outcome.deadlock {
        return Err("deadlock: every guest thread was blocked; the run was killed".into());
    }
    Ok(outcome)
}

/// Fields every guest reports but that describe the whole run
const RUN_WIDE: [&str; 3] = ["switches", "expiries", "schedule_hash"];

/// The aggregated report: run-wide totals, then each process's own fields.
fn print_run_report(o: &RunOutcome) {
    eprintln!("run.processes={}", o.guests.len());
    eprintln!("run.threads={}", o.totals.threads);
    eprintln!("run.switches={}", o.totals.switches);
    eprintln!("run.expiries={}", o.totals.expiries);
    eprintln!("run.schedule_hash={:016x}", o.totals.schedule_hash);
    eprintln!("run.net_connections={}", o.totals.net_connections);
    eprintln!("run.net_datagrams={}", o.totals.net_datagrams);
    eprintln!("run.net_dropped={}", o.totals.net_dropped);
    eprintln!("run.net_bytes={}", o.totals.net_bytes);
    eprintln!("run.net_passthrough={}", o.totals.net_passthrough);
    eprintln!("run.crashes_injected={}", o.totals.crashes_injected);
    eprintln!("run.restarts={}", o.totals.restarts);
    eprintln!("run.clock_ns={}", o.totals.clock_ns);
    if o.totals.stopped_at != 0 {
        eprintln!("run.stopped_at={}", o.totals.stopped_at);
    }
    if o.wall_limited {
        eprintln!("run.wall_limited=true");
    }
    let cpu = |key: &str| -> u64 { o.guests.iter().filter_map(|g| g.report.get_u64(key)).sum() };
    eprintln!("run.cpu_user_ns={}", cpu("cpu_user_ns"));
    eprintln!("run.cpu_system_ns={}", cpu("cpu_system_ns"));
    if let Some((entry, life)) = failed_life(o) {
        eprintln!(
            "run.failure=entry {entry}: {}",
            describe_status(&o.guests[life])
        );
        let at = o.totals.died_at.get(life).copied().unwrap_or(0);
        eprintln!("run.failure_at={at}");
    }
    for (i, g) in o.guests.iter().enumerate() {
        eprintln!("p{i}.status={}", describe_status(g));
        if let Some(&at) = o.totals.died_at.get(i) {
            eprintln!("p{i}.died_at={at}");
        }
        // Lives of one run-file entry share its number
        if let Some(entry) = o.specs[i] {
            eprintln!("p{i}.entry={entry}");
        }
        if let (Some(program), Some(image)) = (&g.program, &g.image) {
            eprintln!("p{i}.program={}", program.display());
            eprintln!("p{i}.image={}", image.display());
        }
        for (k, v) in &g.report.fields {
            if !RUN_WIDE.contains(&k.as_str()) {
                eprintln!("p{i}.{k}={v}");
            }
        }
    }
}

fn describe_status(o: &launch::Outcome) -> String {
    match (o.stopped, o.exit_code(), o.signal()) {
        (true, ..) => "stopped".into(),
        (_, Some(c), _) => format!("exit {c}"),
        (_, None, Some(s)) => format!("signal {s}"),
        _ => "abnormal".into(),
    }
}

/// Exit status of a manifest run: the first of the run file's entries
/// whose last life did not exit 0. Earlier lives that crashed and were
/// restarted do not count, what children return is their parents' business,
/// and daemons and what the run's stop time killed are killed by design.
fn exit_from_run(o: &RunOutcome) -> ExitCode {
    failed_life(o).map_or(ExitCode::SUCCESS, |(_, life)| exit_from(&o.guests[life]))
}

/// The run-file entry that failed the run, and the process that was its
/// last life.
fn failed_life(o: &RunOutcome) -> Option<(usize, usize)> {
    (0..o.initial)
        .filter(|&entry| !o.daemons[entry])
        .filter_map(|entry| Some((entry, o.specs.iter().rposition(|&s| s == Some(entry))?)))
        .find(|&(_, life)| !o.guests[life].stopped && o.guests[life].exit_code() != Some(0))
}

/// The run a tool replays: the command line laid over the run file's own
/// settings, spelled out in full so that every replay is that run.
fn replay_of(
    cli: &Cli,
    tool: &str,
) -> Fallible<(Cli, manifest::Manifest, rewrite::replay::Replay)> {
    if cli.native || !cli.supervisor || !cli.disable_aslr {
        return Err(format!(
            "{tool} replays supervised runs: --native, --no-supervisor and --aslr do not apply"
        )
        .into());
    }
    let path = cli.manifest.clone().unwrap();
    let m = manifest::parse(&std::fs::read_to_string(&path)?)?;
    let cli = with_run_file_settings(cli, &m)?;
    let mut pass = vec![
        "--seed".to_string(),
        cli.opts.seed.to_string(),
        "--quantum".to_string(),
        format!("{}..{}", cli.quantum.0, cli.quantum.1),
        "--mem-hook-rate".to_string(),
        format!("{}/{}", cli.opts.mem_rate.0, cli.opts.mem_rate.1),
        "--net-latency".to_string(),
        format!("{}ns", cli.net_latency_ns),
        "--switch-cost".to_string(),
        format!("{}ns", cli.switch_ns),
        "--heap-size".to_string(),
        cli.heap_size.to_string(),
    ];
    if cli.stop_after_ns != 0 {
        pass.push("--stop-after".to_string());
        pass.push(format!("{}ns", cli.stop_after_ns));
    }
    let replay = rewrite::replay::Replay {
        manifest: path,
        dir: scratch_dir(&cli).join(tool),
        pass,
        // Until the reference run has shown how long a run takes
        timeout: std::time::Duration::from_mins(10),
    };
    Ok((cli, m, replay))
}

/// `derp suspects`: the loads and stores a failing seed needs.
fn suspects(cli: &Cli) -> Fallible<()> {
    let (cli, _, replay) = replay_of(cli, "suspects")?;
    if cli.opts.mem_rate.0 == 0 {
        return Err("no loads or stores are hooked: give --mem-hook-rate".into());
    }
    let mut cfg = rewrite::suspects::Config {
        replay,
        seed: cli.opts.seed,
        jobs: cli.jobs,
    };
    let found = rewrite::suspects::suspects(&mut cfg, |line| println!("{line}"))?;
    println!(
        "{} of {} sites are needed ({} runs):",
        found.suspects.len(),
        found.candidates,
        found.runs
    );
    for s in &found.suspects {
        let (program, kind) = (&s.program, s.kind.name());
        println!("suspect={program} {:#x} {kind} {}", s.addr, s.location);
    }
    Ok(())
}

/// `derp bisect`: find when the failing seed's failure was decided.
fn bisect(cli: &Cli) -> Fallible<()> {
    let (cli, _, replay) = replay_of(cli, "bisect")?;
    let mut cfg = rewrite::bisect::Config {
        replay,
        seed: cli.opts.seed,
        // `--runs` defaults to what `repeat` wants
        runs: if cli.given.contains(&"runs") {
            cli.runs
        } else {
            20
        },
        jobs: cli.jobs,
        resolution_ns: cli.resolution_ns,
    };
    let found = rewrite::bisect::bisect(&mut cfg, |line| println!("{line}"))?;
    if !found.trace.is_empty() {
        println!("switches of the failing run in that interval:");
        for line in found.trace.iter().take(60) {
            println!("  {line}");
        }
    }
    println!("bisect.failure_at_ns={}", found.reference.failure_at);
    println!("bisect.base={}/{}", found.base.failed, cfg.runs);
    println!("bisect.probes={}", found.probes.len());
    println!("bisect.lo_ns={}", found.lo_ns);
    println!("bisect.hi_ns={}", found.hi_ns);
    Ok(())
}

fn exit_from(outcome: &launch::Outcome) -> ExitCode {
    match (outcome.stopped, outcome.exit_code(), outcome.signal()) {
        (true, ..) => ExitCode::SUCCESS,
        (_, Some(c), _) => ExitCode::from(c as u8),
        (_, None, Some(s)) => fail(format!("guest killed by signal {s}")),
        _ => fail("guest ended abnormally"),
    }
}

fn bench(cli: Cli, rest: &[OsString]) -> Fallible<()> {
    let prog = PathBuf::from(&rest[0]);
    let rewritten = cached_rewrite(&prog, &cli.opts)?;
    let mut plain = Cli {
        supervisor: false,
        native: true,
        ..cli
    };
    let time = |exe: &Path, plain: &Cli| -> Fallible<f64> {
        let mut best = f64::INFINITY;
        for _ in 0..3 {
            let t = Instant::now();
            let o = run_guest(exe.to_path_buf(), plain, rest[1..].to_vec(), true, None)?;
            if o.exit_code() != Some(0) {
                return Err(format!("{} failed", exe.display()).into());
            }
            best = best.min(t.elapsed().as_secs_f64());
        }
        Ok(best)
    };
    let native = time(&prog, &plain)?;
    plain.native = false;
    let rewritten_t = time(&rewritten, &plain)?;
    // Native timing has no dylib; the rewritten one loads it passively.
    println!("native    {native:.4}s");
    println!(
        "rewritten {rewritten_t:.4}s  ({:.2}x)",
        rewritten_t / native
    );
    Ok(())
}

/// What must agree between runs: each guest's exit status and stdout, and
/// the run-wide schedule hash.
type Observed = (Vec<(String, String)>, String);

fn observe_single(cli: &Cli, exe: &Path, args: &[OsString], out: &Path) -> Fallible<Observed> {
    let o = run_guest(
        exe.to_path_buf(),
        cli,
        args.to_vec(),
        true,
        Some(out.to_path_buf()),
    )?;
    let text = std::fs::read_to_string(out).unwrap_or_default();
    let hash = o.report.get("schedule_hash").unwrap_or("").to_string();
    Ok((vec![(describe_status(&o), text)], hash))
}

fn observe_manifest(cli: &Cli, manifest: &Path, scratch: &Path) -> Fallible<Observed> {
    let o = run_manifest(cli, manifest, scratch, true)?;
    let guests = o
        .guests
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let text = std::fs::read_to_string(stdout_file(scratch, i)).unwrap_or_default();
            (describe_status(g), text)
        })
        .collect();
    Ok((guests, format!("{:016x}", o.totals.schedule_hash)))
}

fn show(o: &Observed) -> String {
    let guests: Vec<String> =
        o.0.iter()
            .map(|(status, text)| format!("[{status}] stdout={:?}", text.trim_end()))
            .collect();
    format!("hash={} {}", o.1, guests.join(" "))
}

/// Run `cli.runs` times; every run must agree with the first.
fn repeat(cli: &Cli, rest: &[OsString]) -> Fallible<()> {
    let scratch = scratch_dir(cli);
    let single = if cli.manifest.is_none() {
        let exe = cached_rewrite(Path::new(&rest[0]), &cli.opts)?;
        let out = std::env::temp_dir().join(format!("rewrite-repeat-{}", std::process::id()));
        Some((exe, out))
    } else {
        None
    };
    let mut first: Option<Observed> = None;
    let mut result = Ok(());
    for i in 0..cli.runs {
        let this = match (&single, &cli.manifest) {
            (Some((exe, out)), _) => observe_single(cli, exe, &rest[1..], out)?,
            (None, Some(m)) => observe_manifest(cli, m, &scratch)?,
            (None, None) => unreachable!(),
        };
        match &first {
            None => {
                println!("run 0: {}", show(&this));
                first = Some(this);
            }
            Some(f) if *f != this => {
                result = Err(format!("run {i} differs: {}", show(&this)).into());
                break;
            }
            Some(_) => {}
        }
    }
    if let Some((_, out)) = &single {
        let _ = std::fs::remove_file(out);
    }
    if result.is_ok() {
        println!("{} runs identical", cli.runs);
    }
    result
}

/// The variable cargo reads the linker from, for the host's own target
const LINKER_VAR: &str = "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER";

/// `derp cargo …`: cargo with `derp cc` as the linker, through a
/// script (cargo wants a program that takes the linker's arguments). No
/// change to the program's own Cargo.toml or .cargo/config.
fn rooms_cargo(args: Vec<OsString>) -> Fallible<ExitCode> {
    let exe = std::env::current_exe()?;
    let dir = std::env::temp_dir().join(format!("rewrite-cc-{}", unsafe { libc::getuid() }));
    std::fs::create_dir_all(&dir)?;
    let script = dir.join("rewrite-cc");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nexec \"{}\" cc \"$@\"\n", exe.display()),
    )?;
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    let mut cargo = std::process::Command::new("cargo");
    cargo.args(&args);
    if std::env::var_os(LINKER_VAR).is_none() {
        cargo.env(LINKER_VAR, &script);
    }
    let status = cargo.status()?;
    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}

/// `derp rooms <prog>`: the plan `derp cc` would make for it.
fn rooms_plan(prog: &Path) -> Fallible<()> {
    let m = read_macho(prog)?;
    let stats = rw::scan(&m, &Options::default())?;
    let total = stats.branch_sites + stats.call_sites + stats.unreachable_sites;
    println!(
        "sites={total} unreachable={} rooms={} room_bytes={}",
        stats.unreachable_sites, stats.rooms, stats.room_bytes
    );
    let plan = (stats.unreachable_sites > 0)
        .then(|| rewrite::rooms::plan(&m, &stats))
        .flatten();
    match plan {
        None => println!("every site reaches the stubs: no rooms needed"),
        Some(plan) => {
            println!(
                "far_sites={} order_file_lines={}",
                plan.far_sites,
                plan.order_file.lines().count()
            );
            for (name, bytes) in &plan.rooms {
                println!("{name} {bytes} bytes");
            }
        }
    }
    Ok(())
}

/// `derp cc …`: link as `cc` would, then look at the result. An
/// executable with sites out of the stub segment's reach is linked once
/// more, with a room for them in its text (`rooms`). Anything else, and
/// a failed link, is left as it is.
fn rooms_link(args: Vec<OsString>) -> Fallible<ExitCode> {
    let status = std::process::Command::new("cc").args(&args).status()?;
    if !status.success() {
        return Ok(ExitCode::from(status.code().unwrap_or(1) as u8));
    }
    let shared = ["-dynamiclib", "-shared", "-bundle", "-r"];
    let out = args
        .windows(2)
        .find(|w| w[0] == "-o")
        .map(|w| PathBuf::from(&w[1]));
    let Some(out) = out else {
        return Ok(ExitCode::SUCCESS);
    };
    if args.iter().any(|a| shared.iter().any(|s| a == s)) {
        return Ok(ExitCode::SUCCESS);
    }
    let Ok(m) = read_macho(&out) else {
        return Ok(ExitCode::SUCCESS);
    };
    if m.header.filetype != rewrite::macho::MH_EXECUTE {
        return Ok(ExitCode::SUCCESS);
    }
    let Ok(stats) = rw::scan(&m, &Options::default()) else {
        return Ok(ExitCode::SUCCESS);
    };
    let name = out
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    if stats.unreachable_sites == 0 {
        return Ok(ExitCode::SUCCESS);
    }
    if stats.rooms > 0 {
        eprintln!(
            "derp cc: {name}: {} sites out of a b's reach with the {} rooms it has",
            stats.unreachable_sites, stats.rooms
        );
        return Ok(ExitCode::SUCCESS);
    }
    let Some(plan) = rewrite::rooms::plan(&m, &stats) else {
        return Ok(ExitCode::SUCCESS);
    };
    drop(m);
    let dir = PathBuf::from(format!("{}.rooms", out.display()));
    std::fs::create_dir_all(&dir)?;
    let order = dir.join("order.txt");
    std::fs::write(&order, &plan.order_file)?;
    let mut again: Vec<OsString> = args.clone();
    for (symbol, bytes) in &plan.rooms {
        let asm = dir.join(format!("{symbol}.s"));
        let obj = dir.join(format!("{symbol}.o"));
        std::fs::write(&asm, rewrite::rooms::assembly(symbol, *bytes))?;
        let assembled = std::process::Command::new("cc")
            .arg("-c")
            .arg(&asm)
            .arg("-o")
            .arg(&obj)
            .status()?;
        if !assembled.success() {
            return Err(format!("assembling {} failed", asm.display()).into());
        }
        again.push(obj.into());
    }
    again.push(format!("-Wl,-order_file,{}", order.display()).into());
    let relinked = std::process::Command::new("cc").args(&again).status()?;
    if !relinked.success() {
        eprintln!("derp cc: {name}: linking with rooms failed; linked without");
        let status = std::process::Command::new("cc").args(&args).status()?;
        return Ok(ExitCode::from(status.code().unwrap_or(1) as u8));
    }
    let after = read_macho(&out).and_then(|m| Ok(rw::scan(&m, &Options::default())?))?;
    let mb: u64 = plan.rooms.iter().map(|(_, b)| b).sum::<u64>() >> 20;
    let report = format!(
        "derp cc: {name}: {} of {} sites were out of a b's reach; linked again with {} room{} \
         ({mb} MB) in the text; {} still out of reach",
        plan.far_sites,
        stats.branch_sites + stats.call_sites + stats.unreachable_sites,
        plan.rooms.len(),
        if plan.rooms.len() == 1 { "" } else { "s" },
        after.unreachable_sites
    );
    // cargo shows a linker's stderr only when the link fails
    eprintln!("{report}");
    std::fs::write(dir.join("report.txt"), format!("{report}\n"))?;
    Ok(ExitCode::SUCCESS)
}

/// Set by `bisect` and `suspects` on the runs they start: a run whose tool
/// has gone is of no use, and its guests follow it out (`exit_if_orphaned`).
const EXIT_WITH_PARENT: &str = "REWRITE_EXIT_WITH_PARENT";

fn main() -> ExitCode {
    if std::env::var_os(EXIT_WITH_PARENT).is_some() {
        let parent = unsafe { libc::getppid() };
        // Already adopted by launchd: the tool died before we got here
        if parent == 1 {
            unsafe { libc::_exit(1) };
        }
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if unsafe { libc::getppid() } != parent {
                unsafe { libc::_exit(1) };
            }
        });
    }
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.is_empty() {
        return fail(USAGE);
    }
    let cmd = args.remove(0);
    // Their arguments are cargo's and the linker's, not ours
    match cmd.to_str() {
        Some("cargo") => return rooms_cargo(args).unwrap_or_else(fail),
        Some("cc") => return rooms_link(args).unwrap_or_else(fail),
        _ => {}
    }
    let cli = match parse_cli(args) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let rest = cli.rest.clone();
    let result: Fallible<ExitCode> = match cmd.to_str() {
        Some("copy") if rest.len() == 2 => {
            copy(Path::new(&rest[0]), Path::new(&rest[1])).map(|()| ExitCode::SUCCESS)
        }
        Some("scan") if rest.len() == 1 => read_macho(Path::new(&rest[0]))
            .and_then(|m| Ok(rw::scan(&m, &cli.opts)?))
            .map(|stats| {
                println!("{stats}");
                ExitCode::SUCCESS
            }),
        Some("rewrite") if rest.len() == 2 => {
            rewrite_file(Path::new(&rest[0]), Path::new(&rest[1]), &cli.opts).map(|stats| {
                eprintln!("{stats}");
                ExitCode::SUCCESS
            })
        }
        Some("run") if rest.is_empty() && cli.manifest.is_some() => {
            let manifest = cli.manifest.clone().unwrap();
            run_manifest(&cli, &manifest, &scratch_dir(&cli), cli.capture).map(|o| {
                print_run_report(&o);
                exit_from_run(&o)
            })
        }
        Some("run") if !rest.is_empty() && cli.manifest.is_none() => {
            let prog = PathBuf::from(&rest[0]);
            let exe = if cli.native {
                Ok(prog)
            } else {
                cached_rewrite(&prog, &cli.opts)
            };
            exe.and_then(|exe| run_guest(exe, &cli, rest[1..].to_vec(), false, None))
                .map(|o| exit_from(&o))
        }
        Some("bisect") if rest.is_empty() && cli.manifest.is_some() => {
            bisect(&cli).map(|()| ExitCode::SUCCESS)
        }
        Some("suspects") if rest.is_empty() && cli.manifest.is_some() => {
            suspects(&cli).map(|()| ExitCode::SUCCESS)
        }
        Some("bench") if !rest.is_empty() => bench(cli, &rest).map(|()| ExitCode::SUCCESS),
        Some("rooms") if rest.len() == 1 => {
            rooms_plan(Path::new(&rest[0])).map(|()| ExitCode::SUCCESS)
        }
        Some("repeat") if rest.is_empty() != cli.manifest.is_none() => {
            repeat(&cli, &rest).map(|()| ExitCode::SUCCESS)
        }
        _ => return fail(USAGE),
    };
    result.unwrap_or_else(fail)
}
