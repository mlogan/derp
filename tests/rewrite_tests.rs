mod common;

use common::{rewrite_to, run};
use rewrite::launch::{self, Launch};
use rewrite::rewrite::Options;

#[test]
fn loops_branch_only_matches_native() {
    let dir = common::scratch_dir("loops");
    let exe = common::build_c("loops", &dir, &[]);
    let rw_path = dir.join("loops.rw");
    let stats = rewrite_to(&exe, &rw_path, &Options::default());
    assert!(stats.branch_sites > 0 && stats.call_sites > 0, "{stats}");
    assert_eq!(stats.mem_sites, 0);

    let (native, expected) = run(&exe, &["1"], None, 0);
    assert_eq!(native.exit_code(), Some(0));
    assert!(expected.starts_with("primes="));

    let (passive, text) = common::run_passive(&rw_path, &["1"]);
    assert_eq!(passive.exit_code(), Some(0));
    assert_eq!(text, expected);
    assert_eq!(passive.report.get_u64("switches"), None);

    let (supervised, text) = run(&rw_path, &["1"], Some(common::supervisor_dylib()), 0);
    assert_eq!(supervised.exit_code(), Some(0));
    assert_eq!(text, expected);
    let hooks = supervised.report.get_u64("hooks").unwrap();
    let expiries = supervised.report.get_u64("expiries").unwrap();
    assert!(hooks > 1_000_000, "hooks={hooks}");
    assert!(expiries > 100, "expiries={expiries}");
}

#[test]
fn loops_dense_memory_hooks_match_native() {
    let dir = common::scratch_dir("loops_mem");
    let exe = common::build_c("loops", &dir, &[]);
    let (_, expected) = run(&exe, &["1"], None, 0);
    for (seed, rate) in [(1, (1, 16)), (2, (1, 1))] {
        let rw_path = dir.join(format!("loops.rw{seed}"));
        let stats = rewrite_to(
            &exe,
            &rw_path,
            &Options {
                seed,
                mem_rate: rate,
            },
        );
        assert!(stats.mem_sites > 0, "{stats}");
        let (o, text) = run(&rw_path, &["1"], Some(common::supervisor_dylib()), 0);
        assert_eq!(o.exit_code(), Some(0), "seed {seed}");
        assert_eq!(text, expected, "seed {seed}");
    }
}

#[test]
fn stubs_are_slide_proof() {
    let dir = common::scratch_dir("loops_slide");
    let exe = common::build_c("loops", &dir, &[]);
    let rw_path = dir.join("loops.rw");
    rewrite_to(
        &exe,
        &rw_path,
        &Options {
            seed: 3,
            mem_rate: (1, 4),
        },
    );
    let cfg = Launch {
        exe: rw_path.clone(),
        args: vec!["1".into()],
        dylib: Some(common::supervisor_dylib()),
        disable_aslr: false,
        stdout: None,
        seed: 0,
        quantum: launch::DEFAULT_QUANTUM,
        passive: false,
        rewrite: None,
    };
    // Output goes to the test's stdout here; only the status is checked.
    let o = launch::launch(&cfg).unwrap();
    assert_eq!(o.exit_code(), Some(0));
    assert!(o.report.get_u64("hooks").unwrap() > 0);
}

/// A debugger finds a dSYM by the executable's file name, so the rewritten
/// file gets a link to the original's bundle; with it, a source-line
/// breakpoint resolves to the same address as in the original.
#[test]
fn a_rewritten_binary_keeps_its_debug_symbols_reachable() {
    use std::process::Command;
    let dir = common::scratch_dir("debug_symbols");
    let src = common::programs_dir().join("loops.c");
    let exe = dir.join("loops_g");
    let built = Command::new("clang")
        .args(["-g", "-O0", "-o"])
        .arg(&exe)
        .arg(&src)
        .status()
        .unwrap();
    assert!(built.success());
    assert!(dir.join("loops_g.dSYM").is_dir(), "clang -g made no dSYM");

    let out = dir.join("loops_g.rw");
    rewrite::cache::rewrite_file(&exe, &out, &Options::default()).unwrap();
    let link = dir.join("loops_g.rw.dSYM");
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::canonicalize(&link).unwrap(),
        std::fs::canonicalize(dir.join("loops_g.dSYM")).unwrap()
    );
    // Again, and through the cache: idempotent, and the cached copy gets one
    rewrite::cache::rewrite_file(&exe, &out, &Options::default()).unwrap();
    let cached = rewrite::cache::cached_rewrite(&exe, &Options::default()).unwrap();
    let cached_link = std::path::PathBuf::from(format!("{}.dSYM", cached.display()));
    assert!(cached_link.join("Contents").is_dir());
    assert!(!dir.read_dir().unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains("rw-tmp")));

    let resolve = |binary: &std::path::Path| {
        let lldb = Command::new("lldb")
            .args(["-b", "-o", "breakpoint set --file loops.c --line 9"])
            .arg(binary)
            .output();
        let Ok(lldb) = lldb else { return None };
        let text = String::from_utf8_lossy(&lldb.stdout).into_owned();
        let address = text
            .split("address = ")
            .nth(1)?
            .split_whitespace()
            .next()?
            .to_string();
        Some(address)
    };
    let Some(original) = resolve(&exe) else {
        eprintln!("skipped the debugger half: lldb did not resolve the original");
        return;
    };
    assert_eq!(resolve(&out), Some(original.clone()), "rewritten");
    assert_eq!(resolve(&cached), Some(original), "cached");
}
