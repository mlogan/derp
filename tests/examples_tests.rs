//! The examples under `examples/`, run twice each: same seed, same
//! schedule hash and client output. Skipped when the software they need
//! is not installed (see `examples/README.md`).

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;

fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples")
}

fn python_ready() -> bool {
    Path::new("/opt/homebrew/opt/python@3.13/bin/python3.13").exists()
        && examples_dir()
            .join(".venv/lib/python3.13/site-packages")
            .exists()
}

/// Run an example: its schedule hash and the client's output.
fn run_example(name: &str, seed: u64, scratch: &Path) -> (String, String) {
    common::supervisor_dylib();
    let manifest = examples_dir().join(name).join("run.yaml");
    let out = Command::new(common::derp_bin())
        .args([
            "run",
            "-v",
            "--capture",
            "--stop-after",
            "120s",
            "--wall-limit",
            "120s",
        ])
        .arg("--seed")
        .arg(seed.to_string())
        .arg("--scratch")
        .arg(scratch)
        .arg("--manifest")
        .arg(&manifest)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{name}: {err}");
    let hash = err
        .lines()
        .find_map(|l| l.strip_prefix("run.schedule_hash="))
        .expect("hash")
        .to_string();
    // Every process's captured stdout, in order
    let mut stdout = String::new();
    for i in 0.. {
        match std::fs::read_to_string(scratch.join(format!("stdout.{i}"))) {
            Ok(text) => stdout.push_str(&text),
            Err(_) => break,
        }
    }
    (hash, stdout)
}

fn repeats(name: &str, expect: &str) {
    repeats_with(name, expect, true);
}

/// `schedule` false: the output must repeat, the schedule hash is not
/// held to (the Go example's schedule hash takes one of two values).
fn repeats_with(name: &str, expect: &str, schedule: bool) {
    let dir = common::scratch_dir(&format!("example_{name}"));
    let first = run_example(name, 1, &dir.join("a"));
    assert!(first.1.contains(expect), "{name}: {}", first.1);
    let again = run_example(name, 1, &dir.join("b"));
    if schedule {
        assert_eq!(again, first, "{name} did not repeat");
    } else {
        assert_eq!(again.1, first.1, "{name}'s output did not repeat");
    }
    let other = run_example(name, 2, &dir.join("c"));
    assert_ne!(other.0, first.0, "{name}: seed 2 gave seed 1's schedule");
}

#[test]
fn the_redis_example_repeats() {
    if !python_ready() || !Path::new("/opt/homebrew/bin/redis-server").exists() {
        eprintln!("skipped: needs Homebrew's redis and the examples venv");
        return;
    }
    repeats("redis", "hashes: [20, 20, 20, 20]\n");
}

#[test]
fn the_postgres_example_repeats() {
    if !python_ready()
        || !Path::new("/opt/homebrew/opt/postgresql@17/bin/postgres").exists()
        || !examples_dir().join("postgres/data/PG_VERSION").exists()
    {
        eprintln!("skipped: needs Homebrew's postgresql@17, the examples venv and setup.sh");
        return;
    }
    repeats("postgres", "final: [400, 128920, 480]\n");
}

#[test]
fn the_sqlite_example_repeats() {
    if !python_ready() {
        eprintln!("skipped: needs Homebrew's python@3.13 and the examples venv");
        return;
    }
    repeats("sqlite", "800 rows from 4 workers, total 800\n");
}

#[test]
fn the_memcached_example_repeats() {
    if !python_ready() || !Path::new("/opt/homebrew/bin/memcached").exists() {
        eprintln!("skipped: needs Homebrew's memcached and the examples venv");
        return;
    }
    repeats("memcached", "logs: [25, 25, 25, 25]\n");
}

#[test]
fn the_go_example_repeats() {
    if !examples_dir().join("go/bin/kvgo").exists() {
        eprintln!("skipped: build examples/go first (see examples/README.md)");
        return;
    }
    repeats_with("go", "worker 7: ", false);
}
