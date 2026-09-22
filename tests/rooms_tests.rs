//! A program too big for its sites to reach the stubs, linked with rooms
//! for them in its text by `rewrite cc`.

mod common;

use std::process::Command;

#[test]
fn a_big_program_gets_rooms_and_every_site_is_hooked() {
    let dir = common::scratch_dir("rooms");
    let obj = dir.join("bigtext.o");
    let src = common::programs_dir().join("bigtext.s");
    assert!(Command::new("cc")
        .args(["-c"])
        .arg(&src)
        .arg("-o")
        .arg(&obj)
        .status()
        .unwrap()
        .success());
    let plain = dir.join("bigtext_plain");
    assert!(Command::new("cc")
        .arg("-o")
        .arg(&plain)
        .arg(&obj)
        .status()
        .unwrap()
        .success());
    let m = rewrite::macho::MachO::parse(std::fs::read(&plain).unwrap()).unwrap();
    let before = rewrite::rewrite::scan(&m, &rewrite::rewrite::Options::default()).unwrap();
    assert!(before.unreachable_sites >= 2, "{before}");
    assert_eq!(before.rooms, 0);

    // The linker driver: links, sees the sites out of reach, links again
    let exe = dir.join("bigtext");
    let out = Command::new(common::rewrite_bin())
        .arg("cc")
        .arg("-o")
        .arg(&exe)
        .arg(&obj)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("linked again with 1 room"), "{err}");
    assert!(err.contains("; 0 still out of reach"), "{err}");
    assert!(dir.join("bigtext.rooms").join("order.txt").is_file());
    let m = rewrite::macho::MachO::parse(std::fs::read(&exe).unwrap()).unwrap();
    let after = rewrite::rewrite::scan(&m, &rewrite::rewrite::Options::default()).unwrap();
    assert_eq!(after.unreachable_sites, 0, "{after}");
    assert_eq!(after.rooms, 1);
    assert!(after.room_bytes > 0);

    let rw = dir.join("bigtext.rw");
    common::rewrite_to(&exe, &rw, &rewrite::rewrite::Options::default());
    let (o, text) = common::run(&rw, &[], Some(common::supervisor_dylib()), 1);
    assert_eq!(o.exit_code(), Some(0), "{text}");
    assert_eq!(text, "count 50000\n");
    assert!(o.report.get_u64("expiries").unwrap() > 0);
    assert!(o.report.get_u64("hooks").unwrap() >= 100_000);
}

/// A small program is left as it is: no rooms, no second link.
#[test]
fn a_small_program_is_linked_once() {
    let dir = common::scratch_dir("rooms_small");
    let src = common::programs_dir().join("sleeper.c");
    let exe = dir.join("sleeper");
    let out = Command::new(common::rewrite_bin())
        .args(["cc", "-O1", "-o"])
        .arg(&exe)
        .arg(&src)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!dir.join("sleeper.rooms").exists());
}
