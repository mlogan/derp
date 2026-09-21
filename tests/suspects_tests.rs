//! Site minimisation: `lost_update.c` has one unprotected read-modify-write
//! among decoys. The suspects must be on that source line.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use rewrite::rewrite::{sites_from_text, Options, SiteKind};

const RUN_FILE: &str = "hosts:\n  - name: a\n    processes:\n      - lost_update\n";

fn setup(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = common::scratch_dir(name);
    // Line numbers come from the dSYM that -g leaves next to the binary
    common::build_c("lost_update", &dir, &["-g"]);
    let manifest = dir.join("lu.yaml");
    std::fs::write(&manifest, RUN_FILE).unwrap();
    common::supervisor_dylib();
    (dir.clone(), manifest, dir.join("scratch"))
}

const HOOKS: [&str; 2] = ["--mem-hook-rate", "1"];

fn rewrite_cmd(args: &[&str], scratch: &Path, manifest: &Path) -> Command {
    common::rewrite_cmd(args, &HOOKS, scratch, manifest)
}

fn failing_seed(scratch: &Path, manifest: &Path) -> u64 {
    common::failing_and_passing_seed(&HOOKS, scratch, manifest).0
}

use common::report_fields as fields;

/// `atos` comes with the command line tools that build the test programs
fn have_atos() {
    assert!(
        Command::new("atos").arg("-h").output().is_ok(),
        "atos is missing: install the Xcode command line tools"
    );
}

#[test]
fn the_site_table_names_every_hooked_instruction() {
    let dir = common::scratch_dir("suspects_table");
    let exe = common::build_c("lost_update", &dir, &[]);
    let out = dir.join("lost_update.rw");
    let opts = Options {
        seed: 1,
        mem_rate: (1, 1),
    };
    let stats = rewrite::cache::rewrite_file(&exe, &out, &opts).unwrap();
    let table = std::fs::read_to_string(rewrite::cache::sites_path(&out)).unwrap();
    let sites = sites_from_text(&table);
    assert_eq!(
        sites.len(),
        stats.branch_sites + stats.call_sites + stats.mem_sites
    );
    let memory: Vec<u64> = sites
        .iter()
        .filter(|s| matches!(s.kind, SiteKind::Load | SiteKind::Store))
        .map(|s| s.addr)
        .collect();
    assert_eq!(memory, stats.mem_site_addrs);
    assert!(sites.iter().any(|s| s.kind == SiteKind::Load));
    assert!(sites.iter().any(|s| s.kind == SiteKind::Store));
}

/// A masked site still counts events but never ends a quantum: a mask of
/// sites the run never switched at changes nothing, and a mask of every
/// load and store leaves no switch at one.
#[test]
fn a_mask_moves_switches_and_nothing_else() {
    let (dir, manifest, scratch) = setup("suspects_mask");
    let seed = failing_seed(&scratch, &manifest);
    let run = |mask: &str, trace: &str| {
        let mask_path = dir.join("mask");
        std::fs::write(&mask_path, mask).unwrap();
        let trace_path = dir.join(trace);
        let _ = std::fs::remove_file(&trace_path);
        let out = rewrite_cmd(
            &["run", "--capture", "--seed", &seed.to_string()],
            &scratch,
            &manifest,
        )
        .env("REWRITE_MASK", &mask_path)
        .env("REWRITE_TRACE", &trace_path)
        .output()
        .unwrap();
        let sites: Vec<u64> = std::fs::read_to_string(&trace_path)
            .unwrap()
            .lines()
            .filter_map(|l| l.split(' ').find_map(|w| w.strip_prefix("site=0x")))
            .map(|v| u64::from_str_radix(v, 16).unwrap())
            .collect();
        (
            out.status.success(),
            fields(&String::from_utf8_lossy(&out.stderr)),
            sites,
        )
    };
    let table = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| Some(e.ok()?.path()))
        .find(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            name.starts_with(&format!(
                "lost_update{}{seed}-1of1-",
                rewrite::shared::CACHE_TAG
            )) && p.extension().is_some_and(|e| e == "sites")
        })
        .expect("site table of the cached rewrite");
    let memory: Vec<u64> = sites_from_text(&std::fs::read_to_string(table).unwrap())
        .iter()
        .filter(|s| matches!(s.kind, SiteKind::Load | SiteKind::Store))
        .map(|s| s.yield_pc)
        .collect();
    let lines = |pcs: &[u64]| {
        use std::fmt::Write;
        pcs.iter().fold(String::new(), |mut text, pc| {
            let _ = writeln!(text, "lost_update {pc:x}");
            text
        })
    };

    let (ok, plain, switched) = run("", "plain.trace");
    assert!(!ok);
    let unused: Vec<u64> = memory
        .iter()
        .copied()
        .filter(|pc| !switched.contains(pc))
        .collect();
    assert!(!unused.is_empty() && unused.len() < memory.len());
    let (ok, same, _) = run(&lines(&unused), "unused.trace");
    assert!(!ok);
    assert_eq!(same["run.schedule_hash"], plain["run.schedule_hash"]);

    let (ok, _, switched) = run(&lines(&memory), "all.trace");
    assert!(
        switched.iter().all(|pc| !memory.contains(pc)),
        "a switch at a masked site"
    );
    assert!(
        ok,
        "no switch inside the read-modify-write, and still an update was lost"
    );
}

#[test]
fn the_suspects_are_on_the_racy_line() {
    have_atos();
    let (_, manifest, scratch) = setup("suspects_race");
    let seed = failing_seed(&scratch, &manifest);
    let out = rewrite_cmd(
        &["suspects", "--seed", &seed.to_string()],
        &scratch,
        &manifest,
    )
    .output()
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let source = std::fs::read_to_string(common::programs_dir().join("lost_update.c")).unwrap();
    let line_of = |mark: &str| {
        source
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains(mark))
            .map(|(i, _)| format!("lost_update.c:{})", i + 1))
            .collect::<Vec<_>>()
    };
    let (race, decoys) = (line_of("// RACE"), line_of("// DECOY"));
    assert_eq!((race.len(), decoys.len()), (1, 2));

    let suspects: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("suspect="))
        .collect();
    assert!(!suspects.is_empty() && suspects.len() <= 2, "{text}");
    for s in &suspects {
        assert!(s.ends_with(&race[0]), "not on the racy line: {s}\n{text}");
        assert!(decoys.iter().all(|d| !s.ends_with(d)), "a decoy: {s}");
        assert!(s.contains(" load ") || s.contains(" store "), "{s}");
    }
}

/// `latent.c` loses its update across calls, so it fails with no switch at
/// any load or store. The question then goes to the branches and calls, and
/// the answer is inside the function called between the read and the write.
#[test]
fn a_failure_that_needs_no_memory_switch_is_traced_to_a_branch_or_call() {
    have_atos();
    let dir = common::scratch_dir("suspects_latent");
    common::build_c("latent", &dir, &["-g"]);
    let manifest = dir.join("latent.yaml");
    std::fs::write(&manifest, RUN_FILE.replace("lost_update", "latent")).unwrap();
    common::supervisor_dylib();
    let scratch = dir.join("scratch");
    let seed = failing_seed(&scratch, &manifest);
    let out = rewrite_cmd(
        &["suspects", "--seed", &seed.to_string()],
        &scratch,
        &manifest,
    )
    .output()
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("trying branches and calls"), "{text}");
    let suspects: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("suspect="))
        .collect();
    assert!(!suspects.is_empty(), "{text}");
    for s in &suspects {
        assert!(s.contains(" branch ") || s.contains(" call "), "{s}");
        // `think`, or the loop in `worker` that calls it
        assert!(
            s.contains("think (in latent)") || s.contains("worker (in latent)"),
            "{s}"
        );
    }
}

/// Two entries that run the same binary share its sites: a mask goes by
/// program, and the site is a suspect once.
#[test]
fn two_entries_of_one_program_share_their_sites() {
    have_atos();
    let (dir, _, scratch) = setup("suspects_twice");
    let manifest = dir.join("twice.yaml");
    std::fs::write(
        &manifest,
        "hosts:\n  - name: a\n    processes:\n      - lost_update\n      - lost_update\n",
    )
    .unwrap();
    let seed = failing_seed(&scratch, &manifest);
    let out = rewrite_cmd(
        &["suspects", "--seed", &seed.to_string()],
        &scratch,
        &manifest,
    )
    .output()
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let suspects: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("suspect="))
        .collect();
    assert!(!suspects.is_empty() && suspects.len() <= 2, "{text}");
    assert!(
        suspects.iter().all(|s| s.contains("lost_update.c:19)")),
        "{text}"
    );
}

/// The racy line is in a child the guest starts, which no run-file entry
/// names. Processes are matched to programs by what the run's report says
/// each ran, so the child's sites are candidates like any other's.
#[test]
fn the_suspects_may_be_in_a_guests_own_child() {
    have_atos();
    let (dir, _, scratch) = setup("suspects_child");
    common::build_c("parent_of", &dir, &[]);
    let manifest = dir.join("child.yaml");
    std::fs::write(
        &manifest,
        "hosts:\n  - name: a\n    processes:\n      - parent_of lost_update\n",
    )
    .unwrap();
    let seed = failing_seed(&scratch, &manifest);
    let out = rewrite_cmd(
        &["suspects", "--seed", &seed.to_string()],
        &scratch,
        &manifest,
    )
    .output()
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let suspects: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("suspect="))
        .collect();
    assert!(!suspects.is_empty() && suspects.len() <= 2, "{text}");
    for s in &suspects {
        assert!(s.starts_with("lost_update "), "not the child's: {s}");
        assert!(
            s.ends_with("lost_update.c:19)"),
            "not the racy line: {s}\n{text}"
        );
    }
}
