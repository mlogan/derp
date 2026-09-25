//! Scheduler acceptance: mutex.c and channel.rs are always correct with a
//! stable schedule per seed; race.c never loses updates with branch hooks
//! only, and some seed with sparse memory hooks loses them reproducibly.

mod common;

use std::path::Path;

use common::{rewrite_to, run};
use rewrite::rewrite::Options;

const TWO_N: &str = "total=400000 expected=400000\n";

fn supervised(exe: &Path, args: &[&str], seed: u64) -> (String, String) {
    let (o, text) = run(exe, args, Some(common::supervisor_dylib()), seed);
    assert_eq!(o.exit_code(), Some(0), "seed {seed}: {text}");
    let hash = o
        .report
        .get("schedule_hash")
        .expect("schedule_hash")
        .to_string();
    assert!(o.report.get_u64("threads").unwrap() >= 3);
    (text, hash)
}

#[test]
fn mutex_is_always_correct_and_repeatable() {
    let dir = common::scratch_dir("mutex");
    let exe = common::build_c("mutex", &dir, &[]);
    for seed in 1..=4u64 {
        let rw = dir.join(format!("mutex.rw{seed}"));
        let stats = rewrite_to(
            &exe,
            &rw,
            &Options {
                seed,
                mem_rate: (1, 8),
            },
        );
        assert!(stats.mem_sites > 0 || seed > 2, "{stats}");
        let (text, hash) = supervised(&rw, &[], seed);
        assert_eq!(text, TWO_N, "seed {seed}");
        let (text2, hash2) = supervised(&rw, &[], seed);
        assert_eq!((text2, hash2), (text, hash), "seed {seed} not repeatable");
    }
}

#[test]
fn race_branch_hooks_only_never_loses_updates() {
    let dir = common::scratch_dir("race_branch");
    let exe = common::build_c("race", &dir, &[]);
    let rw = dir.join("race.rw");
    rewrite_to(&exe, &rw, &Options::default());
    for seed in 1..=6u64 {
        let (text, _) = supervised(&rw, &[], seed);
        assert_eq!(text, TWO_N, "seed {seed}");
        let (text, _) = supervised(&rw, &["stack"], seed);
        assert_eq!(text, TWO_N, "stack seed {seed}");
    }
}

fn find_lost_update(exe: &Path, dir: &Path, arg: &[&str]) -> (u64, String, String) {
    for seed in 1..=40u64 {
        let rw = dir.join(format!("race.rw{seed}"));
        let stats = rewrite_to(
            exe,
            &rw,
            &Options {
                seed,
                mem_rate: (1, 16),
            },
        );
        if stats.mem_sites == 0 {
            continue;
        }
        let (text, hash) = supervised(&rw, arg, seed);
        if text != TWO_N {
            return (seed, text, hash);
        }
    }
    panic!("no seed in 1..=40 lost an update");
}

#[test]
fn race_with_sparse_memory_hooks_reproduces_from_a_seed() {
    let dir = common::scratch_dir("race_mem");
    let exe = common::build_c("race", &dir, &[]);
    for arg in [&[][..], &["stack"][..]] {
        let (seed, text, hash) = find_lost_update(&exe, &dir, arg);
        assert!(
            text.starts_with("total=") && !text.starts_with("total=400000"),
            "{text}"
        );
        let rw = dir.join(format!("race.rw{seed}"));
        for _ in 0..5 {
            let (t, h) = supervised(&rw, arg, seed);
            assert_eq!(
                (t, h),
                (text.clone(), hash.clone()),
                "seed {seed} not repeatable"
            );
        }
    }
}

#[test]
fn channel_output_is_correct_and_schedule_is_stable() {
    let dir = common::scratch_dir("channel");
    let exe = common::build_rust("channel", &dir);
    let (native, expected) = run(&exe, &[], None, 0);
    assert_eq!(native.exit_code(), Some(0));
    assert!(expected.contains("per_worker [2000, 2000, 2000]"));
    let rw = dir.join("channel.rw");
    let stats = rewrite_to(
        &exe,
        &rw,
        &Options {
            seed: 1,
            mem_rate: (1, 16),
        },
    );
    assert!(stats.mem_sites > 100 && stats.call_sites > 100, "{stats}");
    let mut hashes = Vec::new();
    for seed in 1..=3u64 {
        let (text, hash) = supervised(&rw, &[], seed);
        assert_eq!(text, expected, "seed {seed}");
        let (text2, hash2) = supervised(&rw, &[], seed);
        assert_eq!(text2, expected);
        assert_eq!(hash, hash2, "seed {seed} not repeatable");
        hashes.push(hash);
    }
    hashes.dedup();
    assert!(hashes.len() > 1, "every seed produced the same schedule");
}

#[test]
fn rwlock_readers_never_see_half_a_write() {
    let dir = common::scratch_dir("rwlock");
    let exe = common::build_c("rwlock", &dir, &[]);
    let expected = "a=40000 b=40000 reads=80000 torn=0 probes=some\n";
    for seed in 1..=4u64 {
        let rw = dir.join(format!("rwlock.rw{seed}"));
        rewrite_to(
            &exe,
            &rw,
            &Options {
                seed,
                mem_rate: (1, 8),
            },
        );
        let (text, hash) = supervised(&rw, &[], seed);
        assert_eq!(text, expected, "seed {seed}");
        let (text2, hash2) = supervised(&rw, &[], seed);
        assert_eq!((text2, hash2), (text, hash), "seed {seed} not repeatable");
    }
}

/// `derp run` prints only the guest's own output unless asked for more.
#[test]
fn run_is_quiet_without_verbose() {
    common::supervisor_dylib();
    let dir = common::scratch_dir("quiet_run");
    let exe = common::build_c("mutex", &dir, &[]);
    let derp = |extra: &[&str]| {
        let out = std::process::Command::new(common::derp_bin())
            .arg("run")
            .args(extra)
            .args(["--seed", "1"])
            .arg(&exe)
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), TWO_N);
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    // A fresh build misses the rewrite cache, which would say what it did
    assert_eq!(derp(&[]), "");
    let report = derp(&["-v"]);
    assert!(report.contains("schedule_hash="), "{report}");
    let _ = std::fs::remove_file(&exe);
    common::build_c("mutex", &dir, &[]);
    let report = derp(&["--verbose"]);
    assert!(report.contains("sites hooked"), "{report}");
}
