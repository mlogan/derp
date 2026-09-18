#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

pub fn rewrite_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rewrite"))
}

pub fn target_dir() -> PathBuf {
    rewrite_bin().parent().unwrap().to_path_buf()
}

/// Build the supervisor cdylib once per test binary; cargo does not build
/// another workspace member's cdylib for our integration tests.
pub fn supervisor_dylib() -> PathBuf {
    static BUILD: Once = Once::new();
    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "rewrite-supervisor"])
            .status()
            .expect("cargo build");
        assert!(status.success(), "building the supervisor failed");
    });
    let p = target_dir().join("librewrite_supervisor.dylib");
    assert!(p.exists(), "{} missing", p.display());
    p
}

pub fn programs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/programs")
}

pub fn scratch_dir(name: &str) -> PathBuf {
    let d = target_dir().join("rewrite-tests").join(name);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Compile a C test program with the link flags the rewriter needs.
pub fn build_c(name: &str, out_dir: &Path, extra: &[&str]) -> PathBuf {
    let src = programs_dir().join(format!("{name}.c"));
    let out = out_dir.join(name);
    let status = Command::new("clang")
        .args([
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-Wl,-headerpad,0x1000",
            "-o",
        ])
        .arg(&out)
        .arg(&src)
        .args(extra)
        .status()
        .expect("clang");
    assert!(status.success(), "compiling {name} failed");
    out
}

/// Build a small C program from inline source.
pub fn build_c_source(name: &str, source: &str, out_dir: &Path) -> PathBuf {
    let src = out_dir.join(format!("{name}.c"));
    std::fs::write(&src, source).unwrap();
    let out = out_dir.join(name);
    let status = Command::new("clang")
        .args(["-O2", "-Wl,-headerpad,0x1000", "-o"])
        .arg(&out)
        .arg(&src)
        .status()
        .expect("clang");
    assert!(status.success(), "compiling {name} failed");
    out
}

/// Compile a Rust test program with rustc directly (std, optimized).
pub fn build_rust(name: &str, out_dir: &Path) -> PathBuf {
    let src = programs_dir().join(format!("{name}.rs"));
    let out = out_dir.join(name);
    let status = Command::new("rustc")
        .args(["-O", "-C", "link-args=-Wl,-headerpad,0x1000", "-o"])
        .arg(&out)
        .arg(&src)
        .status()
        .expect("rustc");
    assert!(status.success(), "compiling {name} failed");
    out
}

/// Rewrite `exe` into `out` and sign it.
pub fn rewrite_to(
    exe: &Path,
    out: &Path,
    opts: &rewrite::rewrite::Options,
) -> rewrite::rewrite::Stats {
    let m = rewrite::macho::MachO::parse(std::fs::read(exe).unwrap()).unwrap();
    let r = rewrite::rewrite::rewrite(&m, opts).unwrap();
    std::fs::write(out, &r.image).unwrap();
    std::fs::set_permissions(out, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    rewrite::macho::adhoc_sign(out).unwrap();
    r.stats
}

/// Run `exe` under the launcher with stdout captured to a file.
/// Returns the outcome and the captured stdout.
pub fn run(
    exe: &Path,
    args: &[&str],
    dylib: Option<PathBuf>,
    seed: u64,
) -> (rewrite::launch::Outcome, String) {
    run_mode(exe, args, dylib, seed, false)
}

/// Run a rewritten binary with its stubs live but no scheduler.
pub fn run_passive(exe: &Path, args: &[&str]) -> (rewrite::launch::Outcome, String) {
    run_mode(exe, args, Some(supervisor_dylib()), 0, true)
}

fn run_mode(
    exe: &Path,
    args: &[&str],
    dylib: Option<PathBuf>,
    seed: u64,
    passive: bool,
) -> (rewrite::launch::Outcome, String) {
    let tag = format!("{}-{:?}", std::process::id(), std::thread::current().id());
    let tag = tag.replace(|c: char| !c.is_ascii_alphanumeric(), "");
    let out_path = exe.with_extension(format!("out{tag}"));
    let cfg = rewrite::launch::Launch {
        exe: exe.to_path_buf(),
        args: args.iter().map(std::convert::Into::into).collect(),
        dylib,
        env: Vec::new(),
        disable_aslr: true,
        stdout: Some(out_path.clone()),
        seed,
        quantum: rewrite::launch::DEFAULT_QUANTUM,
        passive,
    };
    let outcome = rewrite::launch::launch(&cfg).expect("launch");
    let text = std::fs::read_to_string(&out_path).unwrap_or_default();
    (outcome, text)
}
