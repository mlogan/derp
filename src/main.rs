use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rewrite::launch::{self, Launch};
use rewrite::macho::{self, MachO};

const USAGE: &str = "\
usage:
  rewrite copy <in> <out>        round-trip a binary through the writer and re-sign it
  rewrite run [opts] <prog> [args…]
      --no-supervisor            do not inject the supervisor dylib
      --aslr                     leave ASLR on
";

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("rewrite: {msg}");
    ExitCode::from(2)
}

fn copy(input: &Path, output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let m = MachO::parse(std::fs::read(input)?)?;
    let out = m.emit(&[], &[], &[])?;
    std::fs::write(output, out)?;
    std::fs::set_permissions(output, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    macho::adhoc_sign(output)?;
    Ok(())
}

fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.is_empty() {
        return fail(USAGE);
    }
    let cmd = args.remove(0);
    match cmd.to_str() {
        Some("copy") if args.len() == 2 => match copy(Path::new(&args[0]), Path::new(&args[1])) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(e),
        },
        Some("run") => {
            let mut dylib = launch::default_dylib();
            let mut disable_aslr = true;
            while let Some(a) = args.first().and_then(|a| a.to_str()) {
                match a {
                    "--no-supervisor" => dylib = None,
                    "--aslr" => disable_aslr = false,
                    _ if a.starts_with("--") => return fail(format!("unknown option {a}")),
                    _ => break,
                }
                args.remove(0);
            }
            if args.is_empty() {
                return fail(USAGE);
            }
            let exe = PathBuf::from(args.remove(0));
            let cfg = Launch {
                exe,
                args,
                dylib,
                env: Vec::new(),
                disable_aslr,
            };
            match launch::launch(&cfg) {
                Ok(outcome) => {
                    for (k, v) in &outcome.report.fields {
                        eprintln!("{k}={v}");
                    }
                    match (outcome.exit_code(), outcome.signal()) {
                        (Some(c), _) => ExitCode::from(c as u8),
                        (None, Some(s)) => fail(format!("guest killed by signal {s}")),
                        _ => fail("guest ended abnormally"),
                    }
                }
                Err(e) => fail(e),
            }
        }
        _ => fail(USAGE),
    }
}
