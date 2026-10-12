//! Perturbations a seed explores beyond the uniform pick: thread pauses.

mod common;

use std::path::Path;
use std::process::Command;

/// One `derp run -v` of a program: its stdout and the report's fields.
fn derp_run(exe: &Path, seed: u64, extra: &[&str]) -> (String, common::RunReport) {
    common::supervisor_dylib();
    let out = Command::new(common::derp_bin())
        .args(["run", "-v", "--seed", &seed.to_string()])
        .args(extra)
        .arg(exe)
        .output()
        .expect("derp run");
    assert!(
        out.status.success(),
        "seed {seed}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let fields = common::report_fields(&String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    (
        stdout,
        common::RunReport {
            fields,
            stdout: Vec::new(),
        },
    )
}

fn unfenced(stdout: &str) -> u64 {
    stdout
        .trim()
        .strip_prefix("unfenced_writes=")
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("unexpected output {stdout:?}"))
}

/// A holder that stalls between checking its lease and writing is what
/// pauses are for: the uniform pick never leaves it out long enough for the
/// lease to run out, and a pause often does.
#[test]
fn pauses_find_a_write_after_the_lease_ran_out() {
    let dir = common::scratch_dir("chaos_lease");
    let exe = common::build_c("lease", &dir, &[]);
    for seed in 1..=10 {
        let (out, r) = derp_run(&exe, seed, &[]);
        assert_eq!(unfenced(&out), 0, "seed {seed} without pauses");
        assert_eq!(r.u64("pauses"), 0);
    }
    let pauses = ["--pause", "1/100", "--pause-for", "10us..100ms"];
    let found: Vec<u64> = (1..=10)
        .filter(|&seed| unfenced(&derp_run(&exe, seed, &pauses).0) > 0)
        .collect();
    assert!(found.len() >= 3, "only seeds {found:?} found it");
    // A seed that finds it finds it again, with the same schedule
    let seed = found[0];
    let (a, ra) = derp_run(&exe, seed, &pauses);
    let (b, rb) = derp_run(&exe, seed, &pauses);
    assert_eq!(a, b);
    assert!(unfenced(&a) > 0);
    assert_eq!(ra.fields["schedule_hash"], rb.fields["schedule_hash"]);
    assert_eq!(ra.u64("pauses"), rb.u64("pauses"));
    assert!(ra.u64("pauses") > 0);
}

#[test]
fn a_run_file_sets_pauses_and_the_command_line_wins() {
    let dir = common::scratch_dir("chaos_runfile");
    common::build_c("lease", &dir, &[]);
    let manifest = dir.join("lease.yaml");
    std::fs::write(
        &manifest,
        "pause: {chance: 1/100, for: 1ms..50ms}\nhosts:\n  - name: a\n    processes:\n      - lease\n",
    )
    .unwrap();
    let scratch = dir.join("scratch");
    let a = common::run_manifest(&manifest, &scratch, 2, 1);
    let b = common::run_manifest(&manifest, &scratch, 2, 1);
    assert!(a.u64("run.pauses") > 0);
    assert_eq!(a.u64("run.pauses"), b.u64("run.pauses"));
    assert_eq!(a.fields["run.schedule_hash"], b.fields["run.schedule_hash"]);
    assert_eq!(a.stdout, b.stdout);
    let off = common::run_manifest_with(&manifest, &scratch, 2, 1, &["--pause", "off"]);
    assert_eq!(off.u64("run.pauses"), 0);
    assert_eq!(off.stdout[0], "unfenced_writes=0\n");
}
