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
