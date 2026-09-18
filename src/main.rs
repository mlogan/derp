use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use rewrite::cache::{cached_rewrite, read_macho, rewrite_file, write_exe, Fallible};
use rewrite::launch::{self, Guest, Launch, Run, RunOutcome};
use rewrite::manifest;
use rewrite::rewrite::{self as rw, Options};

const USAGE: &str = "\
usage:
  rewrite copy <in> <out>              round-trip a binary through the writer and re-sign it
  rewrite scan [opts] <prog>           print what the rewriter would hook
  rewrite rewrite [opts] <in> <out>    rewrite and sign
  rewrite run [opts] <prog> [args…]    rewrite (cached), then launch under the supervisor
  rewrite bench [opts] <prog> [args…]  time native vs rewritten (no supervisor)
  rewrite repeat [opts] <prog> [args…] run N times; exit status, stdout and schedule hash must agree
  rewrite run|repeat [opts] --manifest FILE
                                       several processes under one scheduler; see below
options:
  --runs N                             repetitions for repeat (default 100)
  --seed S                             run seed (default 0)
  --mem-hook-rate R                    0, 1 or a fraction like 1/16 (default 0)
  --quantum LO..HI                     hook events per quantum (default 1000..10000)
  --no-supervisor                      no scheduling: the dylib only provides the stubs' counter
  --aslr                               leave ASLR on
  --native                             run the original binary without the dylib
  --manifest FILE                      processes to start, by virtual host
  --scratch DIR                        working directory and TMPDIR of a manifest run
                                       (default: a directory under the system temp dir)
  --net-latency T                      virtual-time delay between different hosts, such as
                                       5ms, 250us or 1s (default 0)
manifest:
  host NAME                            opens a host
      prog arg \"two words\"           one process; the tokens are its argv verbatim
  Processes start in file order. Program paths are relative to the manifest.
";

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("rewrite: {msg}");
    ExitCode::from(2)
}

struct Cli {
    opts: Options,
    quantum: (u32, u32),
    supervisor: bool,
    manifest: Option<PathBuf>,
    scratch: Option<PathBuf>,
    net_latency_ns: u64,
    disable_aslr: bool,
    native: bool,
    runs: u32,
    rest: Vec<OsString>,
}

/// `5ms`, `250us`, `10ns`, `1s`; a bare number is milliseconds.
fn parse_duration_ns(s: &str) -> Option<u64> {
    let digits = s.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let scale = match &s[digits.len()..] {
        "ns" => 1,
        "us" => 1_000,
        "" | "ms" => 1_000_000,
        "s" => 1_000_000_000,
        _ => return None,
    };
    digits.parse::<u64>().ok()?.checked_mul(scale)
}

fn parse_cli(mut args: Vec<OsString>) -> Result<Cli, String> {
    let mut cli = Cli {
        opts: Options::default(),
        quantum: launch::DEFAULT_QUANTUM,
        supervisor: true,
        manifest: None,
        scratch: None,
        net_latency_ns: 0,
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
            }
            "--mem-hook-rate" => {
                let v = take_value(&mut args)?;
                cli.opts.mem_rate = rw::parse_rate(&v).ok_or(format!("bad rate {v}"))?;
            }
            "--quantum" => {
                let v = take_value(&mut args)?;
                cli.quantum = v
                    .split_once("..")
                    .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
                    .filter(|&(lo, hi): &(u32, u32)| lo >= 1 && hi >= lo)
                    .ok_or(format!("bad quantum {v}"))?;
            }
            "--manifest" => cli.manifest = Some(take_value(&mut args)?.into()),
            "--scratch" => cli.scratch = Some(take_value(&mut args)?.into()),
            "--net-latency" => {
                let v = take_value(&mut args)?;
                cli.net_latency_ns = parse_duration_ns(&v).ok_or(format!("bad duration {v}"))?;
            }
            "--no-supervisor" => cli.supervisor = false,
            "--aslr" => cli.disable_aslr = false,
            "--native" => cli.native = true,
            "--runs" => {
                cli.runs = take_value(&mut args)?.parse().map_err(|_| "bad --runs")?;
            }
            _ if a.starts_with("--") => return Err(format!("unknown option {a}")),
            _ => break,
        }
        args.remove(0);
    }
    cli.rest = args;
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
        .ok_or_else(|| "supervisor dylib not found next to the rewrite binary".into())
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
        env: Vec::new(),
        disable_aslr: cli.disable_aslr,
        stdout,
        seed: cli.opts.seed,
        quantum: cli.quantum,
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
                "{} exists, is not empty and was not created by rewrite",
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

fn scratch_dir(cli: &Cli) -> PathBuf {
    cli.scratch
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join(format!("rewrite-run-{}", std::process::id())))
}

fn stdout_file(scratch: &Path, index: usize) -> PathBuf {
    scratch.join(format!("stdout.{index}"))
}

/// Start the manifest's processes under one scheduler. With `capture`,
/// each guest's stdout goes to `stdout.<index>` in the scratch directory.
fn run_manifest(cli: &Cli, path: &Path, scratch: &Path, capture: bool) -> Fallible<RunOutcome> {
    let m = manifest::parse(&std::fs::read_to_string(path)?)?;
    let base = path.parent().unwrap_or(Path::new("."));
    let mut guests = Vec::new();
    for (i, p) in m.processes.iter().enumerate() {
        let prog = std::fs::canonicalize(base.join(&p.argv[0]))
            .map_err(|e| format!("{}: {e}", p.argv[0]))?;
        let exe = if cli.native {
            prog
        } else {
            cached_rewrite(&prog, &cli.opts)?
        };
        guests.push(Guest {
            exe,
            argv0: Some(p.argv[0].clone().into()),
            args: p.argv[1..].iter().map(Into::into).collect(),
            host: p.host,
            stdout: capture.then(|| stdout_file(scratch, i)),
        });
    }
    prepare_scratch(scratch)?;
    let scratch = std::fs::canonicalize(scratch)?;
    let run = Run {
        guests,
        hosts: m.hosts.clone(),
        dylib: dylib_for(cli)?,
        env: vec![("TMPDIR".into(), scratch.to_string_lossy().into_owned())],
        disable_aslr: cli.disable_aslr,
        seed: cli.opts.seed,
        quantum: cli.quantum,
        cwd: Some(scratch),
        passive: !cli.supervisor,
        rewrite: (!cli.native).then(|| cli.opts.clone()),
        net_latency_ns: cli.net_latency_ns,
    };
    let outcome = launch::launch_run(&run)?;
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
    for (i, g) in o.guests.iter().enumerate() {
        eprintln!("p{i}.status={}", describe_status(g));
        for (k, v) in &g.report.fields {
            if !RUN_WIDE.contains(&k.as_str()) {
                eprintln!("p{i}.{k}={v}");
            }
        }
    }
}

fn describe_status(o: &launch::Outcome) -> String {
    match (o.exit_code(), o.signal()) {
        (Some(c), _) => format!("exit {c}"),
        (None, Some(s)) => format!("signal {s}"),
        _ => "abnormal".into(),
    }
}

/// Exit status of a manifest run: the first of the manifest's own
/// processes that did not exit 0. What their children return is their
/// business.
fn exit_from_run(o: &RunOutcome) -> ExitCode {
    o.guests[..o.initial]
        .iter()
        .find(|g| g.exit_code() != Some(0))
        .map_or(ExitCode::SUCCESS, exit_from)
}

fn exit_from(outcome: &launch::Outcome) -> ExitCode {
    match (outcome.exit_code(), outcome.signal()) {
        (Some(c), _) => ExitCode::from(c as u8),
        (None, Some(s)) => fail(format!("guest killed by signal {s}")),
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

fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.is_empty() {
        return fail(USAGE);
    }
    let cmd = args.remove(0);
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
            run_manifest(&cli, &manifest, &scratch_dir(&cli), false).map(|o| {
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
        Some("bench") if !rest.is_empty() => bench(cli, &rest).map(|()| ExitCode::SUCCESS),
        Some("repeat") if rest.is_empty() != cli.manifest.is_none() => {
            repeat(&cli, &rest).map(|()| ExitCode::SUCCESS)
        }
        _ => return fail(USAGE),
    };
    result.unwrap_or_else(fail)
}
