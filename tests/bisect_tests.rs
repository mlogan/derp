//! Seed bisection: `latent.c` loses an update in a window early in the run
//! and only notices at the very end. Bisection must point at the window.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

const RUN_FILE: &str = "hosts:\n  - name: a\n    processes:\n      - latent\n";

fn rewrite(args: &[&str], scratch: &Path, manifest: &Path) -> std::process::Output {
    common::supervisor_dylib();
    Command::new(common::rewrite_bin())
        .args(args)
        .arg("--scratch")
        .arg(scratch)
        .arg("--manifest")
        .arg(manifest)
        .output()
        .expect("rewrite")
}

fn fields(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The first seed on which the guest aborts
fn failing_seed(scratch: &Path, manifest: &Path) -> u64 {
    (1..=200)
        .find(|seed| {
            !rewrite(
                &["run", "--capture", "--seed", &seed.to_string()],
                scratch,
                manifest,
            )
            .status
            .success()
        })
        .expect("no seed of 200 loses an update")
}

fn switches(trace: &Path) -> Vec<(u64, String)> {
    std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(|l| {
            let (what, clock) = l.rsplit_once(" clock=").unwrap();
            (clock.parse().unwrap(), what.to_string())
        })
        .collect()
}

#[test]
fn a_reseeded_run_is_the_plain_run_until_the_reseed() {
    let dir = common::scratch_dir("bisect_reseed");
    common::build_c("latent", &dir, &[]);
    let manifest = dir.join("latent.yaml");
    std::fs::write(&manifest, RUN_FILE).unwrap();
    common::supervisor_dylib();
    let traced = |extra: &[&str], name: &str| {
        let trace = dir.join(name);
        let _ = std::fs::remove_file(&trace);
        let out = Command::new(common::rewrite_bin())
            .args(["run", "--capture", "--seed", "3"])
            .args(extra)
            .arg("--scratch")
            .arg(dir.join("scratch"))
            .arg("--manifest")
            .arg(&manifest)
            .env("REWRITE_TRACE", &trace)
            .output()
            .unwrap();
        (
            switches(&trace),
            fields(&String::from_utf8_lossy(&out.stderr)),
        )
    };
    let at = 60_000_000;
    let (plain, _) = traced(&[], "plain.trace");
    let (one, f1) = traced(&["--reseed-at", "60ms", "--reseed", "1"], "one.trace");
    let (again, f1_again) = traced(&["--reseed-at", "60ms", "--reseed", "1"], "again.trace");
    let (two, f2) = traced(&["--reseed-at", "60ms", "--reseed", "2"], "two.trace");

    let before = |t: &[(u64, String)]| {
        t.iter()
            .filter(|(c, _)| *c < at)
            .cloned()
            .collect::<Vec<_>>()
    };
    assert!(before(&plain).len() > 10);
    assert_eq!(before(&one), before(&plain), "the past changed");
    assert_eq!(before(&two), before(&plain), "the past changed");
    assert_ne!(one, plain, "the future did not change");
    assert_ne!(one, two, "two replacement seeds, one future");
    assert_eq!(one, again, "a reseeded run must repeat");
    assert_eq!(f1["run.schedule_hash"], f1_again["run.schedule_hash"]);
    assert_ne!(f1["run.schedule_hash"], f2["run.schedule_hash"]);
}

#[test]
fn bisection_points_at_the_race_and_not_at_the_assert() {
    let dir = common::scratch_dir("bisect_latent");
    common::build_c("latent", &dir, &[]);
    let manifest = dir.join("latent.yaml");
    std::fs::write(&manifest, RUN_FILE).unwrap();
    let scratch = dir.join("scratch");
    let seed = failing_seed(&scratch, &manifest);

    let out = rewrite(
        &["bisect", "--seed", &seed.to_string()],
        &scratch,
        &manifest,
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let f = fields(&text);
    let num = |k: &str| {
        f[k].parse::<u64>()
            .unwrap_or_else(|_| panic!("{k} in {text}"))
    };
    let (lo, hi, failed_at) = (
        num("bisect.lo_ns"),
        num("bisect.hi_ns"),
        num("bisect.failure_at_ns"),
    );

    // The guest says when its window was, on its own clock, which runs a
    // fixed offset ahead of the run's: its last line and the run's failure
    // time are the same moment.
    let said = std::fs::read_to_string(scratch.join("bisect/reference/stdout.0")).unwrap();
    let last_number = |l: &str| l.rsplit(' ').next().unwrap().parse::<u64>().unwrap();
    let offset = last_number(said.lines().last().unwrap()) - failed_at;
    let windows: Vec<(u64, u64)> = said
        .lines()
        .filter(|l| l.contains("window:"))
        .map(|l| {
            let mut n = l.rsplit(' ').map(|v| v.parse::<u64>().unwrap() - offset);
            let to = n.next().unwrap();
            (n.next().unwrap(), to)
        })
        .collect();
    let from = windows.iter().map(|w| w.0).min().unwrap();
    let to = windows.iter().map(|w| w.1).max().unwrap();

    let slack = 3_000_000;
    assert!(
        hi + slack >= from && lo <= to + slack,
        "decided in {lo}..{hi}, but the window was {from}..{to}\n{text}"
    );
    assert!(
        hi < failed_at / 2,
        "the assert at {failed_at} is not the moment\n{text}"
    );
    assert!(hi - lo <= 2_000_000, "{text}");
    assert!(
        text.contains("switches of the failing run in that interval"),
        "{text}"
    );
}

#[test]
fn a_seed_that_passes_has_nothing_to_bisect() {
    let dir = common::scratch_dir("bisect_passing");
    common::build_c("latent", &dir, &[]);
    let manifest = dir.join("latent.yaml");
    std::fs::write(&manifest, RUN_FILE).unwrap();
    let scratch = dir.join("scratch");
    let failing = failing_seed(&scratch, &manifest);
    let passing = (1..=200).find(|&s| s != failing).unwrap();
    let ok = rewrite(
        &["run", "--capture", "--seed", &passing.to_string()],
        &scratch,
        &manifest,
    );
    if !ok.status.success() {
        return;
    }
    let out = rewrite(
        &["bisect", "--seed", &passing.to_string()],
        &scratch,
        &manifest,
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("does not fail"));
}

/// Failures decided by a draw from a per-process stream, not by the
/// schedule: where heap blocks land, and what `arc4random` returns. Those
/// streams are reseeded at the probe time too, or every future would fail
/// and there would be no moment to find.
#[test]
fn bisection_finds_a_draw_from_a_process_stream() {
    for mode in ["heap", "entropy"] {
        let dir = common::scratch_dir(&format!("bisect_late_{mode}"));
        common::build_c("late_draw", &dir, &[]);
        let manifest = dir.join("late.yaml");
        std::fs::write(
            &manifest,
            format!("hosts:\n  - name: a\n    processes:\n      - late_draw {mode}\n"),
        )
        .unwrap();
        let scratch = dir.join("scratch");
        let seed = failing_seed(&scratch, &manifest);

        let out = rewrite(
            &["bisect", "--seed", &seed.to_string()],
            &scratch,
            &manifest,
        );
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "{mode}: {text}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let f = fields(&text);
        let num = |k: &str| {
            f[k].parse::<u64>()
                .unwrap_or_else(|_| panic!("{k} in {text}"))
        };
        let (lo, hi, failed_at) = (
            num("bisect.lo_ns"),
            num("bisect.hi_ns"),
            num("bisect.failure_at_ns"),
        );

        let said = std::fs::read_to_string(scratch.join("bisect/reference/stdout.0")).unwrap();
        let numbers =
            |l: &str| -> Vec<u64> { l.split(' ').filter_map(|w| w.parse().ok()).collect() };
        let offset = numbers(said.lines().last().unwrap()).pop().unwrap() - failed_at;
        let draw = numbers(said.lines().find(|l| l.starts_with("draw:")).unwrap());
        let (from, to) = (draw[0] - offset, draw[1] - offset);

        let slack = 2_000_000;
        assert!(
            hi + slack >= from && lo <= to + slack,
            "{mode}: decided in {lo}..{hi}, but the draw was at {from}..{to}\n{text}"
        );
        assert!(hi < failed_at / 2 && hi - lo <= 2_000_000, "{mode}: {text}");
    }
}
