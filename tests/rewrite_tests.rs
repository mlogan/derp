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

    let (standalone, text) = run(&rw_path, &["1"], None, 0);
    assert_eq!(standalone.exit_code(), Some(0));
    assert_eq!(text, expected);

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
        env: vec![],
        disable_aslr: false,
        stdout: None,
        seed: 0,
        quantum: launch::DEFAULT_QUANTUM,
    };
    // Output goes to the test's stdout here; only the status is checked.
    let o = launch::launch(&cfg).unwrap();
    assert_eq!(o.exit_code(), Some(0));
    assert!(o.report.get_u64("hooks").unwrap() > 0);
}
