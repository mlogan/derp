use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use rewrite::launch::{self, Launch};
use rewrite::macho::{self, MachO};
use rewrite::rewrite::{self as rw, Options};

const USAGE: &str = "\
usage:
  rewrite copy <in> <out>              round-trip a binary through the writer and re-sign it
  rewrite scan [opts] <prog>           print what the rewriter would hook
  rewrite rewrite [opts] <in> <out>    rewrite and sign
  rewrite run [opts] <prog> [args…]    rewrite (cached), then launch under the supervisor
  rewrite bench [opts] <prog> [args…]  time native vs rewritten (no supervisor)
options:
  --seed S                             run seed (default 0)
  --mem-hook-rate R                    0, 1 or a fraction like 1/16 (default 0)
  --quantum LO..HI                     hook events per quantum (default 1000..10000)
  --no-supervisor                      run the rewritten binary without the dylib
  --aslr                               leave ASLR on
  --native                             run the original binary without the dylib
";

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("rewrite: {msg}");
    ExitCode::from(2)
}

struct Cli {
    opts: Options,
    quantum: String,
    supervisor: bool,
    disable_aslr: bool,
    native: bool,
    rest: Vec<OsString>,
}

fn parse_cli(mut args: Vec<OsString>) -> Result<Cli, String> {
    let mut cli = Cli {
        opts: Options::default(),
        quantum: "1000..10000".into(),
        supervisor: true,
        disable_aslr: true,
        native: false,
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
            "--quantum" => cli.quantum = take_value(&mut args)?,
            "--no-supervisor" => cli.supervisor = false,
            "--aslr" => cli.disable_aslr = false,
            "--native" => cli.native = true,
            _ if a.starts_with("--") => return Err(format!("unknown option {a}")),
            _ => break,
        }
        args.remove(0);
    }
    cli.rest = args;
    Ok(cli)
}

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

fn write_exe(path: &Path, image: &[u8]) -> Fallible<()> {
    std::fs::write(path, image)?;
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    macho::adhoc_sign(path)?;
    Ok(())
}

fn read_macho(path: &Path) -> Fallible<MachO> {
    Ok(MachO::parse(std::fs::read(path)?)?)
}

fn copy(input: &Path, output: &Path) -> Fallible<()> {
    write_exe(output, &read_macho(input)?.emit(&[], &[], &[])?)
}

fn do_rewrite(input: &Path, output: &Path, opts: &Options) -> Fallible<rw::Stats> {
    let r = rw::rewrite(&read_macho(input)?, opts)?;
    write_exe(output, &r.image)?;
    Ok(r.stats)
}

/// Rewrite into a cache file next to the program, keyed by options and
/// the input's modification time.
fn cached_rewrite(input: &Path, opts: &Options) -> Fallible<PathBuf> {
    let mtime = std::fs::metadata(input)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let name = format!(
        "{}.rw-{}-{}of{}-{mtime}",
        input.file_name().unwrap_or_default().to_string_lossy(),
        opts.seed,
        opts.mem_rate.0,
        opts.mem_rate.1
    );
    let out = input.with_file_name(name);
    if !out.exists() {
        let stats = do_rewrite(input, &out, opts)?;
        eprintln!(
            "rewrite: {} sites hooked -> {}",
            stats.branch_sites + stats.call_sites + stats.mem_sites,
            out.display()
        );
    }
    Ok(out)
}

fn run_guest(
    exe: PathBuf,
    cli: &Cli,
    args: Vec<OsString>,
    quiet: bool,
) -> Fallible<launch::Outcome> {
    let dylib = if cli.supervisor && !cli.native {
        Some(
            launch::default_dylib()
                .ok_or("supervisor dylib not found next to the rewrite binary")?,
        )
    } else {
        None
    };
    let cfg = Launch {
        exe,
        args,
        dylib,
        env: vec![
            ("REWRITE_SEED".into(), cli.opts.seed.to_string()),
            ("REWRITE_QUANTUM".into(), cli.quantum.clone()),
        ],
        disable_aslr: cli.disable_aslr,
    };
    let outcome = launch::launch(&cfg)?;
    if !quiet {
        for (k, v) in &outcome.report.fields {
            eprintln!("{k}={v}");
        }
    }
    Ok(outcome)
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
            let o = run_guest(exe.to_path_buf(), plain, rest[1..].to_vec(), true)?;
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
    println!("native    {native:.4}s");
    println!(
        "rewritten {rewritten_t:.4}s  ({:.2}x)",
        rewritten_t / native
    );
    Ok(())
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
            do_rewrite(Path::new(&rest[0]), Path::new(&rest[1]), &cli.opts).map(|stats| {
                eprintln!("{stats}");
                ExitCode::SUCCESS
            })
        }
        Some("run") if !rest.is_empty() => {
            let prog = PathBuf::from(&rest[0]);
            let exe = if cli.native {
                Ok(prog)
            } else {
                cached_rewrite(&prog, &cli.opts)
            };
            exe.and_then(|exe| run_guest(exe, &cli, rest[1..].to_vec(), false))
                .map(|o| exit_from(&o))
        }
        Some("bench") if !rest.is_empty() => bench(cli, &rest).map(|()| ExitCode::SUCCESS),
        _ => return fail(USAGE),
    };
    result.unwrap_or_else(fail)
}
