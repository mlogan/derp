//! The command line: help, and mistakes answered with what was wrong
//! rather than ignored.

mod common;

use std::path::Path;
use std::process::{Command, Output};

fn derp(args: &[&str], dir: &Path) -> Output {
    common::supervisor_dylib();
    Command::new(common::derp_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn help_and_version_go_to_stdout_and_succeed() {
    let dir = common::scratch_dir("cli_help");
    for args in [
        &["--help"][..],
        &["-h"],
        &["run", "--help"],
        &["bisect", "-h"],
    ] {
        let out = derp(args, &dir);
        assert!(out.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("usage:"));
    }
    let out = derp(&["--version"], &dir);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("derp "));
}

#[test]
fn mistakes_say_what_is_wrong() {
    let dir = common::scratch_dir("cli_mistakes");
    let prog = common::build_c("race", &dir, &[]);
    let prog = prog.to_str().unwrap();
    let script = dir.join("script.sh");
    std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
    let cases: [(&[&str], &str); 13] = [
        (&["run", "-s", "3", prog], "unknown option -s"),
        (&["run", "--seed", "x", prog], "bad seed x (a whole number"),
        (&["run", "--wall-limit", "0s", prog], "bad wall-limit 0s"),
        (&["run", "--quantum", "10", prog], "(LO..HI"),
        (&["run", "--switch-cost", "1ms", prog], "without a run file"),
        (&["run", "--capture", prog], "without a run file"),
        (
            &["scan", "--runs", "3", prog],
            "--runs does nothing for derp scan",
        ),
        (
            &["run", "--native", "--stop-after", "1s", prog],
            "needs the supervisor",
        ),
        (&["bisect", prog], "replays a failing run file"),
        (&["frob"], "unknown command frob"),
        (&["run", "/bin/ls"], "universal (fat) binary"),
        (&["run", script.to_str().unwrap()], "a script"),
        (
            &["run", "--manifest", "nonexistent.yaml"],
            "run file nonexistent.yaml",
        ),
    ];
    for (args, expected) in cases {
        let out = derp(args, &dir);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(
            stderr(&out).contains(expected),
            "{args:?}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn rewrite_refuses_to_overwrite_its_input() {
    let dir = common::scratch_dir("cli_rewrite_in_place");
    let prog = common::build_c("race", &dir, &[]);
    let before = std::fs::read(&prog).unwrap();
    let p = prog.to_str().unwrap();
    let out = derp(&["rewrite", p, p], &dir);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("other than the input"));
    assert_eq!(std::fs::read(&prog).unwrap(), before);
}

#[test]
fn one_program_is_reported_as_a_run_and_a_signal_is_named() {
    let dir = common::scratch_dir("cli_single_report");
    let prog = common::build_c_source(
        "aborts",
        "#include <stdlib.h>\nint main(void) { abort(); }\n",
        &dir,
    );
    let out = derp(&["run", "-v", prog.to_str().unwrap()], &dir);
    assert_eq!(out.status.code(), Some(128 + libc::SIGABRT));
    let err = stderr(&out);
    assert!(err.contains("guest killed by signal 6 (SIGABRT)"), "{err}");
    let fields = common::report_fields(&err);
    assert!(fields.contains_key("run.clock_ns"), "{err}");
    assert_eq!(fields["p0.status"], "signal 6");
}
