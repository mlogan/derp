mod common;

use rewrite::launch::{self, Launch};
use rewrite::macho::{self, MachO};

const HELLO: &str = "#include <stdio.h>\nint main(){puts(\"hi\");return 0;}\n";

fn run_capture(
    exe: &std::path::Path,
    dylib: Option<std::path::PathBuf>,
) -> (launch::Outcome, String) {
    // Route the guest's stdout through a file so the test can read it.
    let out_path = exe.with_extension("out");
    let script = common::scratch_dir("macho").join("run.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nexec \"$@\" > {}\n", out_path.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let cfg = Launch {
        exe: script,
        args: vec![exe.as_os_str().to_owned()],
        dylib,
        env: Vec::new(),
        disable_aslr: true,
    };
    let outcome = launch::launch(&cfg).expect("launch");
    let text = std::fs::read_to_string(&out_path).unwrap_or_default();
    (outcome, text)
}

#[test]
fn parses_hello_world() {
    let dir = common::scratch_dir("macho");
    let exe = common::build_c_source("hello_parse", HELLO, &dir);
    let m = MachO::parse(std::fs::read(&exe).unwrap()).unwrap();
    assert!(m.segment("__TEXT").is_some());
    assert!(m.segment("__LINKEDIT").is_some());
    let text = m.text_section().unwrap();
    let starts = m.function_starts().unwrap();
    assert!(!starts.is_empty());
    assert!(starts
        .iter()
        .all(|&a| a >= text.addr && a < text.addr + text.size));
    assert!(m.header_room() >= 0x1000);
}

#[test]
fn round_trip_runs_standalone_and_supervised() {
    let dir = common::scratch_dir("macho");
    let exe = common::build_c_source("hello", HELLO, &dir);
    let m = MachO::parse(std::fs::read(&exe).unwrap()).unwrap();
    let out = m
        .emit(&[], &[1, 2, 3, 4], &[0xd6, 0x5f, 0x03, 0xc0])
        .unwrap();
    let rw = dir.join("hello_rt.rw");
    std::fs::write(&rw, out).unwrap();
    std::fs::set_permissions(&rw, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    macho::adhoc_sign(&rw).unwrap();

    let m2 = MachO::parse(std::fs::read(&rw).unwrap()).unwrap();
    let layout = m.plan_layout().unwrap();
    assert_eq!(
        m2.segment(macho::STUB_DATA_SEGMENT).unwrap().vmaddr,
        layout.data_addr
    );
    assert_eq!(
        m2.segment(macho::STUB_TEXT_SEGMENT).unwrap().vmaddr,
        layout.text_addr
    );
    assert_eq!(m2.read_word(layout.data_addr), Some(0x0403_0201));
    assert_eq!(m2.read_word(layout.text_addr), Some(0xc003_5fd6));
    assert_eq!(m2.function_starts().unwrap(), m.function_starts().unwrap());

    let (outcome, text) = run_capture(&rw, None);
    assert_eq!(outcome.exit_code(), Some(0));
    assert_eq!(text, "hi\n");

    let (outcome, text) = run_capture(&rw, Some(common::supervisor_dylib()));
    assert_eq!(outcome.exit_code(), Some(0));
    assert_eq!(text, "hi\n");
    assert_eq!(outcome.report.get("supervisor"), Some("loaded"));
}

#[test]
fn refuses_binary_without_header_room() {
    let dir = common::scratch_dir("macho");
    let src = dir.join("tight.c");
    std::fs::write(&src, HELLO).unwrap();
    let exe = dir.join("tight");
    assert!(std::process::Command::new("clang")
        .args(["-O2", "-o"])
        .arg(&exe)
        .arg(&src)
        .status()
        .unwrap()
        .success());
    let m = MachO::parse(std::fs::read(&exe).unwrap()).unwrap();
    let err = m.emit(&[], &[], &[]).unwrap_err();
    assert!(matches!(err, macho::Error::NoHeaderRoom { .. }), "{err}");
}
