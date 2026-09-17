mod common;

use std::path::{Path, PathBuf};

use rewrite::launch::{self, Launch};
use rewrite::macho::{self, MachO};
use rewrite::rewrite::{self as rw, Options};

fn rewrite_to(exe: &Path, out: &Path, opts: &Options) -> rw::Stats {
    let m = MachO::parse(std::fs::read(exe).unwrap()).unwrap();
    let r = rw::rewrite(&m, opts).unwrap();
    std::fs::write(out, &r.image).unwrap();
    std::fs::set_permissions(out, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    macho::adhoc_sign(out).unwrap();
    r.stats
}

/// Run `exe` with stdout captured to a file; returns (outcome, stdout).
fn run(
    exe: &Path,
    args: &[&str],
    dylib: Option<PathBuf>,
    env: Vec<(String, String)>,
) -> (launch::Outcome, String) {
    let out_path = exe.with_extension(format!("out{}", std::process::id()));
    let script = exe.with_extension("sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nexec \"$@\" > {}\n", out_path.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let mut all = vec![exe.as_os_str().to_owned()];
    all.extend(args.iter().map(std::convert::Into::into));
    let cfg = Launch {
        exe: script,
        args: all,
        dylib,
        env,
        disable_aslr: true,
    };
    let outcome = launch::launch(&cfg).expect("launch");
    let text = std::fs::read_to_string(&out_path).unwrap_or_default();
    (outcome, text)
}

#[test]
fn loops_branch_only_matches_native() {
    let dir = common::scratch_dir("loops");
    let exe = common::build_c("loops", &dir, &[]);
    let rw_path = dir.join("loops.rw");
    let stats = rewrite_to(&exe, &rw_path, &Options::default());
    assert!(stats.branch_sites > 0 && stats.call_sites > 0, "{stats}");
    assert_eq!(stats.mem_sites, 0);

    let (native, expected) = run(&exe, &["1"], None, vec![]);
    assert_eq!(native.exit_code(), Some(0));
    assert!(expected.starts_with("primes="));

    let (standalone, text) = run(&rw_path, &["1"], None, vec![]);
    assert_eq!(standalone.exit_code(), Some(0));
    assert_eq!(text, expected);

    let (supervised, text) = run(&rw_path, &["1"], Some(common::supervisor_dylib()), vec![]);
    assert_eq!(supervised.exit_code(), Some(0));
    assert_eq!(text, expected);
    let hooks = supervised.report.get_u64("hooks").unwrap();
    let switches = supervised.report.get_u64("switches").unwrap();
    assert!(hooks > 1_000_000, "hooks={hooks}");
    assert!(switches > 100, "switches={switches}");
}

#[test]
fn loops_dense_memory_hooks_match_native() {
    let dir = common::scratch_dir("loops_mem");
    let exe = common::build_c("loops", &dir, &[]);
    let (_, expected) = run(&exe, &["1"], None, vec![]);
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
        let (o, text) = run(&rw_path, &["1"], Some(common::supervisor_dylib()), vec![]);
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
    };
    // Output goes to the test's stdout here; only the status is checked.
    let o = launch::launch(&cfg).unwrap();
    assert_eq!(o.exit_code(), Some(0));
    assert!(o.report.get_u64("hooks").unwrap() > 0);
}
