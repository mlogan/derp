#![allow(dead_code)]

use std::collections::BTreeMap;
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
        .args([
            "--edition",
            "2021",
            "-O",
            "-C",
            "link-args=-Wl,-headerpad,0x1000",
            "-o",
        ])
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
        disable_aslr: true,
        stdout: Some(out_path.clone()),
        seed,
        quantum: rewrite::launch::DEFAULT_QUANTUM,
        passive,
        rewrite: None,
    };
    let outcome = rewrite::launch::launch(&cfg).expect("launch");
    let text = std::fs::read_to_string(&out_path).unwrap_or_default();
    (outcome, text)
}

pub struct RunReport {
    pub fields: BTreeMap<String, String>,
    pub stdout: Vec<String>,
}

impl RunReport {
    pub fn u64(&self, key: &str) -> u64 {
        self.fields
            .get(key)
            .unwrap_or_else(|| panic!("{key} missing from {:?}", self.fields))
            .parse()
            .unwrap()
    }
}

/// One `rewrite run --capture`: the guests' stdout from the scratch
/// directory and the aggregated report from the launcher's stderr.
pub fn run_manifest(manifest: &Path, scratch: &Path, seed: u64, guests: usize) -> RunReport {
    run_manifest_with(manifest, scratch, seed, guests, &[])
}

pub fn run_manifest_with(
    manifest: &Path,
    scratch: &Path,
    seed: u64,
    guests: usize,
    extra: &[&str],
) -> RunReport {
    supervisor_dylib();
    let report = Command::new(rewrite_bin())
        .args(["run", "--capture", "--seed", &seed.to_string()])
        .args(extra)
        .arg("--scratch")
        .arg(scratch)
        .arg("--manifest")
        .arg(manifest)
        .output()
        .expect("rewrite run");
    assert!(
        report.status.success(),
        "seed {seed}: {}",
        String::from_utf8_lossy(&report.stderr)
    );
    let stdout = (0..guests)
        .map(|i| std::fs::read_to_string(scratch.join(format!("stdout.{i}"))).unwrap())
        .collect();
    let fields = String::from_utf8_lossy(&report.stderr)
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    RunReport { fields, stdout }
}

/// Build the tokio guest (`tests/programs/kv`, a package of its own) once
/// per test binary and copy it into `out_dir`.
pub fn build_kv(out_dir: &Path) -> PathBuf {
    static BUILD: Once = Once::new();
    let build = target_dir().join("rewrite-tests/kv-build");
    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args(["build", "--locked", "--manifest-path"])
            .arg(programs_dir().join("kv/Cargo.toml"))
            .arg("--target-dir")
            .arg(&build)
            .status()
            .expect("cargo build");
        assert!(status.success(), "building kv failed");
    });
    // A new file each time: macOS kills a process whose executable was
    // overwritten in place under a cached code signature
    let (out, fresh) = (out_dir.join("kv"), out_dir.join("kv.new"));
    std::fs::copy(build.join("debug/kv"), &fresh).unwrap();
    std::fs::rename(&fresh, &out).unwrap();
    out
}

/// `key=value` lines of a report or of a tool's output
pub fn report_fields(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// `rewrite <args> <extra> --scratch … --manifest …`
pub fn rewrite_cmd(args: &[&str], extra: &[&str], scratch: &Path, manifest: &Path) -> Command {
    supervisor_dylib();
    let mut cmd = Command::new(rewrite_bin());
    cmd.args(args)
        .args(extra)
        .arg("--scratch")
        .arg(scratch)
        .arg("--manifest")
        .arg(manifest);
    cmd
}

/// The first seed from 1 on which the run fails with a guest's own status
/// (not a launcher error), and a seed on which it passes.
pub fn failing_and_passing_seed(extra: &[&str], scratch: &Path, manifest: &Path) -> (u64, u64) {
    let (mut failing, mut passing) = (None, None);
    for seed in 1..=200u64 {
        let out = rewrite_cmd(
            &["run", "--capture", "--seed", &seed.to_string()],
            extra,
            scratch,
            manifest,
        )
        .output()
        .unwrap();
        let report = String::from_utf8_lossy(&out.stderr).into_owned();
        if out.status.success() {
            passing.get_or_insert(seed);
        } else {
            assert!(report.contains("run.failure="), "seed {seed}: {report}");
            failing.get_or_insert(seed);
        }
        if let (Some(f), Some(p)) = (failing, passing) {
            return (f, p);
        }
    }
    panic!("200 seeds: failing {failing:?}, passing {passing:?}");
}
