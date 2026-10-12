//! What a seed explores beyond the uniform pick: thread pauses and the PCT
//! scheduler.

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

/// The number a test program prints as `name=N`
fn printed(stdout: &str, name: &str) -> u64 {
    stdout
        .trim()
        .strip_prefix(name)
        .and_then(|rest| rest.strip_prefix('='))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("expected {name}=N, got {stdout:?}"))
}

fn unfenced(stdout: &str) -> u64 {
    printed(stdout, "unfenced_writes")
}

/// Seeds among `seeds` whose run prints a non-zero `name`
fn finding_seeds(
    exe: &Path,
    seeds: std::ops::RangeInclusive<u64>,
    extra: &[&str],
    name: &str,
) -> Vec<u64> {
    seeds
        .filter(|&seed| printed(&derp_run(exe, seed, extra).0, name) > 0)
        .collect()
}

/// A holder that stalls between checking its lease and writing is what
/// pauses are for: the uniform pick never leaves it out long enough for the
/// lease to run out, and a pause often does.
#[test]
fn pauses_find_a_write_after_the_lease_ran_out() {
    let dir = common::scratch_dir("chaos_lease");
    let exe = common::build_c("lease", &dir, &[]);
    for seed in 1..=10 {
        let (out, r) = derp_run(&exe, seed, &["--pause", "off"]);
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

/// One ordering constraint: PCT at depth 1 finds it with probability at
/// least 1/n for n threads (here 3) without any change point.
#[test]
fn pct_finds_an_ordering_bug_as_often_as_its_bound_says() {
    let dir = common::scratch_dir("chaos_order");
    let exe = common::build_c("order", &dir, &[]);
    let pct = [
        "--scheduler",
        "pct",
        "--pct-depth",
        "1",
        "--pct-horizon",
        "1s",
    ];
    let found = finding_seeds(&exe, 1..=21, &pct, "saw_uninitialized");
    assert!(found.len() >= 7, "only seeds {found:?} of 21");
}

/// Two ordering constraints: PCT at depth 2 needs its change point in the
/// user's window, so the horizon must be about the run's length (70µs of
/// virtual time here). Its bound is 1/(n·k) for k scheduling points.
#[test]
fn pct_finds_an_atomicity_violation_with_a_horizon_the_length_of_the_run() {
    let dir = common::scratch_dir("chaos_atomicity");
    let exe = common::build_c("atomicity", &dir, &[]);
    let pct = [
        "--scheduler",
        "pct",
        "--pct-depth",
        "2",
        "--pct-horizon",
        "50us",
    ];
    let found = finding_seeds(&exe, 1..=40, &pct, "used_after_close");
    assert!(found.len() >= 4, "only seeds {found:?} of 40");
    let (a, ra) = derp_run(&exe, found[0], &pct);
    let (b, rb) = derp_run(&exe, found[0], &pct);
    assert_eq!(a, b);
    assert_eq!(ra.fields["schedule_hash"], rb.fields["schedule_hash"]);
    assert_eq!(ra.u64("pct_changes"), 1);
    // A horizon far past the run's end puts no change point in it
    let late = [
        "--scheduler",
        "pct",
        "--pct-depth",
        "2",
        "--pct-horizon",
        "10s",
    ];
    assert!(finding_seeds(&exe, 1..=40, &late, "used_after_close").len() < found.len());
}

#[test]
fn pct_needs_a_horizon_and_a_run_file_can_choose_it() {
    let dir = common::scratch_dir("chaos_pct_runfile");
    let exe = common::build_c("lease", &dir, &[]);
    common::supervisor_dylib();
    let out = Command::new(common::derp_bin())
        .args(["run", "--scheduler", "pct"])
        .arg(&exe)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--pct-horizon"));
    let manifest = dir.join("lease.yaml");
    std::fs::write(
        &manifest,
        "scheduler: pct\npct: {depth: 3, horizon: 20ms}\nhosts:\n  - name: a\n    processes:\n      - lease\n",
    )
    .unwrap();
    let scratch = dir.join("scratch");
    let a = common::run_manifest(&manifest, &scratch, 6, 1);
    let b = common::run_manifest(&manifest, &scratch, 6, 1);
    assert_eq!(a.u64("run.pct_changes"), 2);
    assert_eq!(a.fields["run.schedule_hash"], b.fields["run.schedule_hash"]);
    assert_eq!(a.stdout, b.stdout);
    let random = common::run_manifest_with(&manifest, &scratch, 6, 1, &["--scheduler", "random"]);
    assert_eq!(random.u64("run.pct_changes"), 0);
    assert_ne!(
        random.fields["run.schedule_hash"],
        a.fields["run.schedule_hash"]
    );
}

#[test]
fn pauses_are_on_by_default() {
    let dir = common::scratch_dir("chaos_default");
    let exe = common::build_c("lease", &dir, &[]);
    let found = (1..=10)
        .filter(|&seed| {
            let (out, r) = derp_run(&exe, seed, &[]);
            assert!(r.u64("pauses") > 0, "seed {seed}");
            unfenced(&out) > 0
        })
        .count();
    assert!(found > 0);
}
