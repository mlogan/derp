mod common;

use rewrite::launch::{self, Launch};
use rewrite::macho::{self, MachO};

const HELLO: &str = "#include <stdio.h>\nint main(){puts(\"hi\");return 0;}\n";

fn run_capture(
    exe: &std::path::Path,
    dylib: Option<std::path::PathBuf>,
) -> (launch::Outcome, String) {
    let out_path = exe.with_extension("out");
    let cfg = Launch {
        exe: exe.to_path_buf(),
        args: Vec::new(),
        dylib,
        disable_aslr: true,
        stdout: Some(out_path.clone()),
        seed: 0,
        quantum: launch::DEFAULT_QUANTUM,
        passive: false,
        rewrite: None,
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
    // `ret`: never reached, only mapped
    let out = m.emit(&[], &[0xc0, 0x03, 0x5f, 0xd6]).unwrap();
    let rw = dir.join("hello_rt.rw");
    std::fs::write(&rw, out).unwrap();
    std::fs::set_permissions(&rw, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    macho::adhoc_sign(&rw).unwrap();

    let m2 = MachO::parse(std::fs::read(&rw).unwrap()).unwrap();
    let layout = m.plan_layout().unwrap();
    let stub = m2.segment(macho::STUB_TEXT_SEGMENT).unwrap();
    assert_eq!(stub.vmaddr, layout.text_addr);
    assert!(stub.sections.is_empty());
    assert_eq!(m2.read_word(layout.text_addr), Some(0xd65f_03c0));
    assert_eq!(m2.function_starts().unwrap(), m.function_starts().unwrap());

    let (outcome, text) = run_capture(&rw, None);
    assert_eq!(outcome.exit_code(), Some(0));
    assert_eq!(text, "hi\n");

    let (outcome, text) = run_capture(&rw, Some(common::supervisor_dylib()));
    assert_eq!(outcome.exit_code(), Some(0));
    assert_eq!(text, "hi\n");
    assert_eq!(outcome.report.get("supervisor"), Some("loaded"));
}

fn build_default_linked(name: &str, dir: &std::path::Path) -> std::path::PathBuf {
    let src = dir.join(format!("{name}.c"));
    std::fs::write(&src, HELLO).unwrap();
    let exe = dir.join(name);
    assert!(std::process::Command::new("clang")
        .args(["-O2", "-o"])
        .arg(&exe)
        .arg(&src)
        .status()
        .unwrap()
        .success());
    exe
}

#[test]
fn default_linked_hello_rewrites_and_runs() {
    let dir = common::scratch_dir("macho_tight");
    let exe = build_default_linked("tight", &dir);
    let m = MachO::parse(std::fs::read(&exe).unwrap()).unwrap();
    assert!(
        m.header_room() < 72,
        "toolchain now pads headers: {}",
        m.header_room()
    );

    let rw = dir.join("tight.rw");
    let stats = common::rewrite_to(&exe, &rw, &rewrite::rewrite::Options::default());
    assert!(stats.branch_sites + stats.call_sites > 0, "{stats}");
    let m2 = MachO::parse(std::fs::read(&rw).unwrap()).unwrap();
    assert!(m2.command(macho::LC_SOURCE_VERSION).is_none());
    assert!(m2.command(macho::LC_UUID).is_some());
    assert!(m2.command(macho::LC_CODE_SIGNATURE).is_some());

    let (outcome, text) = run_capture(&rw, Some(common::supervisor_dylib()));
    assert_eq!(outcome.exit_code(), Some(0));
    assert_eq!(text, "hi\n");
    assert!(outcome.report.get_u64("hooks").unwrap() > 0);
}

#[test]
fn roomy_binary_keeps_its_optional_commands() {
    let dir = common::scratch_dir("macho_roomy");
    let exe = common::build_c_source("roomy", HELLO, &dir);
    let rw = dir.join("roomy.rw");
    common::rewrite_to(&exe, &rw, &rewrite::rewrite::Options::default());
    let m2 = MachO::parse(std::fs::read(&rw).unwrap()).unwrap();
    assert!(m2.command(macho::LC_FUNCTION_STARTS).is_some());
    assert!(m2.command(macho::LC_SOURCE_VERSION).is_some());
}
