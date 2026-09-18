//! Multi-process acceptance: guests listed in a manifest run under one
//! scheduler, and the run-wide schedule is a function of the seed.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

struct RunReport {
    fields: BTreeMap<String, String>,
    stdout: Vec<String>,
}

impl RunReport {
    fn u64(&self, key: &str) -> u64 {
        self.fields
            .get(key)
            .unwrap_or_else(|| panic!("{key} missing from {:?}", self.fields))
            .parse()
            .unwrap()
    }
}

/// One `rewrite repeat` run for the guests' captured stdout, then one
/// `rewrite run` for the aggregated report.
fn run_manifest(manifest: &Path, scratch: &Path, seed: u64, guests: usize) -> RunReport {
    common::supervisor_dylib();
    let out = Command::new(common::rewrite_bin())
        .args([
            "repeat",
            "--runs",
            "1",
            "--seed",
            &seed.to_string(),
            "--scratch",
        ])
        .arg(scratch)
        .arg("--manifest")
        .arg(manifest)
        .output()
        .expect("rewrite repeat");
    assert!(
        out.status.success(),
        "seed {seed}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = (0..guests)
        .map(|i| std::fs::read_to_string(scratch.join(format!("stdout.{i}"))).unwrap())
        .collect();
    let report = Command::new(common::rewrite_bin())
        .args(["run", "--seed", &seed.to_string(), "--scratch"])
        .arg(scratch)
        .arg("--manifest")
        .arg(manifest)
        .output()
        .expect("rewrite run");
    assert!(report.status.success());
    let fields = String::from_utf8_lossy(&report.stderr)
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    RunReport { fields, stdout }
}

#[test]
fn two_loops_processes_share_one_schedule() {
    let dir = common::scratch_dir("multiproc_loops");
    let exe = common::build_c("loops", &dir, &[]);
    let native = Command::new(&exe).arg("1").output().unwrap();
    let expected = String::from_utf8(native.stdout).unwrap();
    let manifest = dir.join("two.manifest");
    std::fs::write(&manifest, "host a\n    loops 1\nhost b\n    loops 1\n").unwrap();
    let scratch = dir.join("scratch");

    let mut hashes = Vec::new();
    for seed in 1..=3u64 {
        let r = run_manifest(&manifest, &scratch, seed, 2);
        assert_eq!(
            r.stdout,
            [expected.clone(), expected.clone()],
            "seed {seed}"
        );
        assert_eq!(r.u64("run.processes"), 2);
        // Both guests run the same code, so alternation shows as many
        // switches with both processes consuming hooks.
        assert!(r.u64("run.switches") > 100, "{:?}", r.fields);
        assert!(r.u64("p0.hooks") > 0 && r.u64("p1.hooks") > 0);
        assert_eq!(r.fields["p0.shared_fixed"], "true");
        assert_eq!(r.fields["p1.shared_fixed"], "true");
        let again = run_manifest(&manifest, &scratch, seed, 2);
        assert_eq!(
            r.fields["run.schedule_hash"], again.fields["run.schedule_hash"],
            "seed {seed} not repeatable"
        );
        assert_eq!(r.u64("p0.hooks"), again.u64("p0.hooks"));
        hashes.push(r.fields["run.schedule_hash"].clone());
    }
    hashes.dedup();
    assert!(hashes.len() > 1, "every seed produced the same schedule");
}

#[test]
fn a_guest_that_dies_holding_the_baton_does_not_hang_the_run() {
    let dir = common::scratch_dir("multiproc_crash");
    common::build_c_source(
        "crash",
        "#include <stdlib.h>\nint main(){ abort(); }\n",
        &dir,
    );
    common::build_c_source(
        "fine",
        "#include <stdio.h>\nint main(){ for (volatile int i = 0; i < 3000000; i++) {} puts(\"ok\"); return 0; }\n",
        &dir,
    );
    let manifest = dir.join("crash.manifest");
    std::fs::write(&manifest, "host a\n    crash\n    fine\n").unwrap();
    common::supervisor_dylib();
    let out = Command::new(common::rewrite_bin())
        .args(["run", "--seed", "1", "--scratch"])
        .arg(dir.join("scratch"))
        .arg("--manifest")
        .arg(&manifest)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("p0.status=signal 6"), "{err}");
    assert!(err.contains("p1.status=exit 0"), "{err}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
    assert!(!out.status.success());
}
