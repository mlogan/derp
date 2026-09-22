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
        heap_size: rewrite::launch::DEFAULT_HEAP,
        stdout: None,
        stderr: None,
        seed: 0,
        quantum: launch::DEFAULT_QUANTUM,
        stop_at_ns: 0,
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

/// Where heap blocks land is the seed's to decide, like the schedule: how
/// two blocks compare, and whether a freed block comes straight back, must
/// go both ways across seeds and the same way for a seed.
#[test]
fn heap_layout_is_a_function_of_the_seed_and_not_predictable() {
    let dir = common::scratch_dir("ptr_order");
    let exe = common::build_c("ptr_order", &dir, &[]);
    let rw = dir.join("ptr_order.rw");
    rewrite_to(&exe, &rw, &Options::default());
    let dylib = common::supervisor_dylib();

    let mut answers: Vec<std::collections::BTreeSet<String>> = Vec::new();
    for seed in 1..=16u64 {
        let (o, text) = run(&rw, &[], Some(dylib.clone()), seed);
        assert_eq!(o.exit_code(), Some(0), "seed {seed}: {text}");
        let (_, again) = run(&rw, &[], Some(dylib.clone()), seed);
        assert_eq!(again, text, "seed {seed} not repeatable");
        for (i, line) in text.lines().enumerate() {
            if answers.len() <= i {
                answers.push(std::collections::BTreeSet::new());
            }
            answers[i].insert(line.to_string());
        }
    }
    // Four comparisons, each seen as a<b and as a>b
    assert_eq!(answers.len(), 5, "{answers:?}");
    for seen in &answers[..4] {
        assert_eq!(seen.len(), 2, "one way on all 16 seeds: {seen:?}");
    }
    assert!(
        answers[4].iter().all(|l| l.ends_with("sometimes")),
        "{:?}",
        answers[4]
    );

    // The bug that needs a < b is found by some seeds, and found again
    let crashed: Vec<u64> = (1..=16)
        .filter(|&seed| {
            run(&rw, &["crash"], Some(dylib.clone()), seed)
                .0
                .exit_code()
                != Some(0)
        })
        .collect();
    assert!(
        crashed.len() >= 3 && crashed.len() <= 13,
        "crashing seeds: {crashed:?}"
    );
    let (o, _) = run(&rw, &["crash"], Some(dylib.clone()), crashed[0]);
    assert_eq!(o.signal(), Some(libc::SIGABRT), "seed {} again", crashed[0]);
}

/// Corners a review found: an absurd size overflowed into a live block,
/// scattered slabs left no room for a 32 MB block, every `malloc` took
/// 8 KB of stack, and frees in a guest's key destructors were leaked
/// because they ran after the thread had left the schedule.
#[test]
fn allocator_corners() {
    let dir = common::scratch_dir("alloc_edges");
    let exe = common::build_c("alloc_edges", &dir, &[]);
    let rw = dir.join("alloc_edges.rw");
    rewrite_to(&exe, &rw, &Options::default());
    for seed in 1..=3u64 {
        let (o, text) = run(&rw, &[], Some(common::supervisor_dylib()), seed);
        assert_eq!(o.exit_code(), Some(0), "seed {seed}: {text}");
        assert_eq!(
            text,
            "absurd size: null\nkey destructors ran\nmalloc on a small stack: ok\n\
             32 MB after 80 MB of small blocks: ok\n256 MB after 80 MB of small blocks: ok\n\
             2048 MB after 80 MB of small blocks: ok\n",
            "seed {seed}"
        );
        assert_eq!(
            o.report.get_u64("heap_leaked_blocks"),
            Some(0),
            "seed {seed}"
        );
    }
}

/// A constant table in the text that the function table lists without a
/// symbol (hand-written assembly keeps round constants that way): its words
/// decode as instructions, one as a backward `b`, and must not be hooked.
#[test]
fn an_unnamed_constant_table_in_the_text_is_left_alone() {
    let dir = common::scratch_dir("asm_table");
    let exe = common::build_c("asm_table", &dir, &[]);
    let rw_path = dir.join("asm_table.rw");
    let stats = rewrite_to(&exe, &rw_path, &Options::default());
    assert_eq!(stats.unnamed_entries, 1, "{stats}");
    assert!(stats.call_sites > 0, "{stats}");

    let (native, expected) = run(&exe, &[], None, 0);
    assert_eq!(native.exit_code(), Some(0));
    assert_eq!(expected, "7 12648209782\n");
    let (supervised, text) = run(&rw_path, &[], Some(common::supervisor_dylib()), 0);
    assert_eq!(supervised.exit_code(), Some(0));
    assert_eq!(text, expected);
}

/// Where scheduled threads' stacks and mappings land is the run's to
/// decide: in the reserved region, the same on every run, whatever the
/// kernel placed elsewhere meanwhile.
#[test]
fn thread_stacks_and_mappings_land_in_the_region_and_repeat() {
    let dir = common::scratch_dir("stacks");
    let exe = common::build_c("stacks", &dir, &[]);
    let rw_path = dir.join("stacks.rw");
    rewrite_to(&exe, &rw_path, &Options::default());
    let (o, first) = run(&rw_path, &[], Some(common::supervisor_dylib()), 1);
    assert_eq!(o.exit_code(), Some(0), "{first}");
    assert!(o.report.get_u64("mappings_placed").unwrap() >= 5, "{first}");
    assert_eq!(o.report.get_u64("mappings_overflowed"), Some(0));
    let addresses: Vec<u64> = first
        .split(|c: char| !c.is_ascii_hexdigit() && c != 'x')
        .filter_map(|w| w.strip_prefix("0x"))
        .filter_map(|h| u64::from_str_radix(h, 16).ok())
        .collect();
    assert_eq!(addresses.len(), 9, "{first}");
    for a in &addresses {
        assert!(
            (0x7C_0000_0000..0x8C_0000_0000).contains(a),
            "{a:#x} outside the region"
        );
    }
    let (_, again) = run(&rw_path, &[], Some(common::supervisor_dylib()), 1);
    assert_eq!(again, first);
}

/// The kernel frees an exited thread's stack itself, at a moment of real
/// time, and would grant an `mmap` hint into the hole once it has: the run
/// places a hinted request like one without an address.
#[test]
fn a_hinted_mapping_is_placed_by_the_run_not_granted_by_the_kernel() {
    let dir = common::scratch_dir("hint");
    let exe = common::build_c("hint", &dir, &[]);
    let rw_path = dir.join("hint.rw");
    rewrite_to(&exe, &rw_path, &Options::default());
    let (o, first) = run(&rw_path, &[], Some(common::supervisor_dylib()), 1);
    assert_eq!(o.exit_code(), Some(0), "{first}");
    assert!(first.contains(" placed"), "{first}");
    assert_eq!(o.report.get_u64("mappings_hinted"), Some(1), "{first}");
    let (_, again) = run(&rw_path, &[], Some(common::supervisor_dylib()), 1);
    assert_eq!(again, first);
}
