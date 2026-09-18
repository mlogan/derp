//! Multi-process acceptance: guests listed in a manifest run under one
//! scheduler, and the run-wide schedule is a function of the seed.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

struct RunReport {
    fields: BTreeMap<String, String>,
    stdout: Vec<String>,
}

impl RunReport {
    fn u64(&self, key: &str) -> u64 {
        self.fields
            .get(key)
            .unwrap_or_else(|| panic!("{key} missing from {:?}", self.fields))
            .parse()
            .unwrap()
    }
}

/// One `rewrite repeat` run for the guests' captured stdout, then one
/// `rewrite run` for the aggregated report.
fn run_manifest(manifest: &Path, scratch: &Path, seed: u64, guests: usize) -> RunReport {
    common::supervisor_dylib();
    let out = Command::new(common::rewrite_bin())
        .args([
            "repeat",
            "--runs",
            "1",
            "--seed",
            &seed.to_string(),
            "--scratch",
        ])
        .arg(scratch)
        .arg("--manifest")
        .arg(manifest)
        .output()
        .expect("rewrite repeat");
    assert!(
        out.status.success(),
        "seed {seed}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = (0..guests)
        .map(|i| std::fs::read_to_string(scratch.join(format!("stdout.{i}"))).unwrap())
        .collect();
    let report = Command::new(common::rewrite_bin())
        .args(["run", "--seed", &seed.to_string(), "--scratch"])
        .arg(scratch)
        .arg("--manifest")
        .arg(manifest)
        .output()
        .expect("rewrite run");
    assert!(report.status.success());
    let fields = String::from_utf8_lossy(&report.stderr)
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    RunReport { fields, stdout }
}

#[test]
fn two_loops_processes_share_one_schedule() {
    let dir = common::scratch_dir("multiproc_loops");
    let exe = common::build_c("loops", &dir, &[]);
    let native = Command::new(&exe).arg("1").output().unwrap();
    let expected = String::from_utf8(native.stdout).unwrap();
    let manifest = dir.join("two.manifest");
    std::fs::write(&manifest, "host a\n    loops 1\nhost b\n    loops 1\n").unwrap();
    let scratch = dir.join("scratch");

    let mut hashes = Vec::new();
    for seed in 1..=3u64 {
        let r = run_manifest(&manifest, &scratch, seed, 2);
        assert_eq!(
            r.stdout,
            [expected.clone(), expected.clone()],
            "seed {seed}"
        );
        assert_eq!(r.u64("run.processes"), 2);
        // Both guests run the same code, so alternation shows as many
        // switches with both processes consuming hooks.
        assert!(r.u64("run.switches") > 100, "{:?}", r.fields);
        assert!(r.u64("p0.hooks") > 0 && r.u64("p1.hooks") > 0);
        let again = run_manifest(&manifest, &scratch, seed, 2);
        assert_eq!(
            r.fields["run.schedule_hash"], again.fields["run.schedule_hash"],
            "seed {seed} not repeatable"
        );
        assert_eq!(r.u64("p0.hooks"), again.u64("p0.hooks"));
        hashes.push(r.fields["run.schedule_hash"].clone());
    }
    hashes.dedup();
    assert!(hashes.len() > 1, "every seed produced the same schedule");
}

#[test]
fn a_guest_that_dies_holding_the_baton_does_not_hang_the_run() {
    let dir = common::scratch_dir("multiproc_crash");
    common::build_c_source(
        "crash",
        "#include <stdlib.h>\nint main(){ abort(); }\n",
        &dir,
    );
    common::build_c_source(
        "fine",
        "#include <stdio.h>\nint main(){ for (volatile int i = 0; i < 3000000; i++) {} puts(\"ok\"); return 0; }\n",
        &dir,
    );
    let manifest = dir.join("crash.manifest");
    std::fs::write(&manifest, "host a\n    crash\n    fine\n").unwrap();
    common::supervisor_dylib();
    let out = Command::new(common::rewrite_bin())
        .args(["run", "--seed", "1", "--scratch"])
        .arg(dir.join("scratch"))
        .arg("--manifest")
        .arg(&manifest)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("p0.status=signal 6"), "{err}");
    assert!(err.contains("p1.status=exit 0"), "{err}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
    assert!(!out.status.success());
}

/// Output of a single-program manifest run whose processes share stdout.
fn run_spawn_tree(manifest: &Path, scratch: &Path, seed: u64) -> (Vec<String>, String) {
    let r = run_manifest(manifest, scratch, seed, 1);
    let lines = r.stdout[0].lines().map(str::to_string).collect();
    assert_eq!(r.u64("run.processes"), 5, "{:?}", r.fields);
    (lines, r.fields["run.schedule_hash"].clone())
}

#[test]
fn spawned_forked_and_execed_children_join_the_schedule() {
    let dir = common::scratch_dir("multiproc_spawn");
    common::build_c("spawn_tree", &dir, &[]);
    let manifest = dir.join("tree.manifest");
    std::fs::write(&manifest, "host a\n    spawn_tree\n").unwrap();
    let scratch = dir.join("scratch");

    let mut orders = Vec::new();
    for seed in 1..=4u64 {
        let (lines, hash) = run_spawn_tree(&manifest, &scratch, seed);
        assert_eq!(lines[0], "parent pid=1000 ppid=1", "seed {seed}");
        assert!(
            lines.contains(&"spawned 1001 1002 1003 1004".to_string()),
            "{lines:?}"
        );
        assert_eq!(
            lines.last().unwrap().split(" sum=").next(),
            Some("no more children: -1")
        );
        let mut sorted = lines.clone();
        sorted.sort();
        let children: Vec<&String> = sorted.iter().filter(|l| l.starts_with("child")).collect();
        assert_eq!(children.len(), 4, "{lines:?}");
        for (i, line) in children.iter().enumerate() {
            let expect = format!("child {i} pid={} ppid=1000 sum=", 1001 + i);
            assert!(line.starts_with(&expect), "{line}");
        }
        let reaped: Vec<&String> = sorted.iter().filter(|l| l.starts_with("reaped")).collect();
        let expect: Vec<String> = (0..4)
            .map(|i| format!("reaped {} exit={}", 1001 + i, 10 + i))
            .collect();
        assert_eq!(
            reaped.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            expect
        );
        // The first reap names a pid, so it is child 1 whatever the schedule
        let first = lines.iter().find(|l| l.starts_with("reaped")).unwrap();
        assert_eq!(first, "reaped 1002 exit=11");

        let (again, hash2) = run_spawn_tree(&manifest, &scratch, seed);
        assert_eq!(
            (again, hash2),
            (lines.clone(), hash),
            "seed {seed} not repeatable"
        );
        orders.push(lines);
    }
    orders.dedup();
    assert!(
        orders.len() > 1,
        "every seed interleaved the output the same way"
    );
}

#[test]
fn pipeline_is_correct_with_a_stable_schedule() {
    let dir = common::scratch_dir("multiproc_pipeline");
    let exe = common::build_c("pipeline", &dir, &[]);
    let native = Command::new(&exe).output().unwrap();
    let expected = String::from_utf8(native.stdout).unwrap();
    assert!(expected.starts_with("count=66666 checksum="), "{expected}");
    let manifest = dir.join("pipeline.manifest");
    std::fs::write(&manifest, "host a\n    pipeline\n").unwrap();
    let scratch = dir.join("scratch");

    let mut hashes = Vec::new();
    for seed in 1..=3u64 {
        let r = run_manifest(&manifest, &scratch, seed, 1);
        assert_eq!(r.stdout[0], expected, "seed {seed}");
        assert_eq!(r.u64("run.processes"), 4);
        // Every stage had to wait for a pipe, in both directions
        assert!(r.u64("p1.io_waits") > 0, "{:?}", r.fields);
        assert!(r.u64("p2.io_waits") > 0 && r.u64("p3.io_waits") > 0);
        let again = run_manifest(&manifest, &scratch, seed, 1);
        assert_eq!(again.stdout[0], expected);
        assert_eq!(
            r.fields["run.schedule_hash"], again.fields["run.schedule_hash"],
            "seed {seed} not repeatable"
        );
        hashes.push(r.fields["run.schedule_hash"].clone());
    }
    hashes.dedup();
    assert!(hashes.len() > 1, "every seed produced the same schedule");
}

fn expected_echo(connections: usize) -> String {
    use std::fmt::Write;
    // The blob's checksum is masked: it is whatever the run says, and only
    // has to be the same every time.
    let mut out = String::new();
    for c in 0..connections {
        for m in 0..3 {
            writeln!(out, "conn {c} #{m}: hello {m} from connection {c}").unwrap();
        }
        writeln!(out, "conn {c} blob sum=BLOB").unwrap();
    }
    out
}

fn mask_blob(text: &str) -> String {
    text.lines()
        .map(|l| match l.split_once("blob sum=") {
            Some((head, _)) => format!("{head}blob sum=BLOB\n"),
            None => format!("{l}\n"),
        })
        .collect()
}

fn check_echo(name: &str, manifest_text: &str) {
    let dir = common::scratch_dir(name);
    common::build_c("tcp_echo", &dir, &[]);
    let manifest = dir.join("echo.manifest");
    std::fs::write(&manifest, manifest_text).unwrap();
    let scratch = dir.join("scratch");
    let mut hashes = Vec::new();
    let mut blob_sums = Vec::new();
    for seed in 1..=3u64 {
        let r = run_manifest(&manifest, &scratch, seed, 2);
        assert_eq!(
            r.stdout[0], "server done after 4 connections\n",
            "seed {seed}"
        );
        assert_eq!(mask_blob(&r.stdout[1]), expected_echo(4), "seed {seed}");
        blob_sums.push(r.stdout[1].lines().nth(3).unwrap().to_string());
        // Everything went through the virtual network
        assert_eq!(r.u64("run.net_connections"), 4, "{:?}", r.fields);
        assert_eq!(r.u64("run.net_passthrough"), 0);
        assert!(r.u64("run.net_bytes") > 4 * 200 * 1024);
        // 200 KB into a 64 KB ring: the client had to wait for the server
        assert!(r.u64("p1.io_waits") > 0, "{:?}", r.fields);
        let again = run_manifest(&manifest, &scratch, seed, 2);
        assert_eq!(again.stdout, r.stdout);
        assert_eq!(
            r.fields["run.schedule_hash"], again.fields["run.schedule_hash"],
            "seed {seed} not repeatable"
        );
        hashes.push(r.fields["run.schedule_hash"].clone());
    }
    blob_sums.dedup();
    assert_eq!(blob_sums.len(), 1);
    hashes.dedup();
    assert!(hashes.len() > 1, "every seed produced the same schedule");
}

#[test]
fn tcp_echo_between_two_hosts() {
    check_echo(
        "multiproc_tcp",
        "host alpha\n    tcp_echo server 7000 4\nhost beta\n    tcp_echo client alpha 7000 4\n",
    );
}

#[test]
fn unix_domain_echo_on_one_host() {
    check_echo(
        "multiproc_unix",
        "host alpha\n    tcp_echo server --unix /virtual/echo.sock 4\n    tcp_echo client --unix /virtual/echo.sock 4\n",
    );
}

#[test]
fn udp_ping_survives_lost_datagrams() {
    let dir = common::scratch_dir("multiproc_udp");
    common::build_c("udp_ping", &dir, &[]);
    let manifest = dir.join("udp.manifest");
    std::fs::write(
        &manifest,
        "host alpha\n    udp_ping server 5353\nhost beta\n    udp_ping client alpha 5353 20\n",
    )
    .unwrap();
    let scratch = dir.join("scratch");
    let pongs = (0..20).fold(String::new(), |mut all, i| {
        use std::fmt::Write;
        writeln!(all, "pong {i}").unwrap();
        all
    });
    let mut hashes = Vec::new();
    for seed in 1..=4u64 {
        let r = run_manifest(&manifest, &scratch, seed, 2);
        assert_eq!(r.stdout[1], pongs, "seed {seed}");
        let server: Vec<&str> = r.stdout[0].lines().collect();
        assert_eq!(server[0], "server socket type dgram");
        assert_eq!(*server.last().unwrap(), "got \"done\" from 10.0.0.2");
        for i in 0..20 {
            let line = format!("got \"ping {i}\" from 10.0.0.2");
            assert!(
                server.contains(&line.as_str()),
                "seed {seed}: {line} missing"
            );
        }
        assert_eq!(r.u64("run.net_passthrough"), 0);
        assert!(r.u64("run.net_datagrams") >= 41, "{:?}", r.fields);
        let again = run_manifest(&manifest, &scratch, seed, 2);
        assert_eq!(again.stdout, r.stdout, "seed {seed} not repeatable");
        assert_eq!(
            r.fields["run.schedule_hash"],
            again.fields["run.schedule_hash"]
        );
        hashes.push(r.fields["run.schedule_hash"].clone());
    }
    hashes.dedup();
    assert!(hashes.len() > 1, "every seed produced the same schedule");
}

#[test]
fn two_hosts_share_a_port_and_loopback_stays_home() {
    let dir = common::scratch_dir("multiproc_hosts");
    common::build_c("two_hosts", &dir, &[]);
    let manifest = dir.join("hosts.manifest");
    std::fs::write(
        &manifest,
        "host red\n    two_hosts server 8080\nhost blue\n    two_hosts server 8080\n\
         host green\n    two_hosts client 8080 red blue\n",
    )
    .unwrap();
    let scratch = dir.join("scratch");
    for seed in 1..=3u64 {
        let r = run_manifest(&manifest, &scratch, seed, 3);
        assert_eq!(r.stdout[0], "child of 1000 runs on red\n", "seed {seed}");
        assert_eq!(r.stdout[1], "child of 1001 runs on blue\n", "seed {seed}");
        assert_eq!(
            r.stdout[2],
            "client on green\n\
             interface lo0 127.0.0.1 loopback\n\
             interface en0 10.0.0.3\n\
             by name red: red at 10.0.0.1:8080 greets 10.0.0.3\n\
             by address 10.0.0.1: red at 10.0.0.1:8080 greets 10.0.0.3\n\
             bind to 10.0.0.1: not available\n\
             by name blue: blue at 10.0.0.2:8080 greets 10.0.0.3\n\
             by address 10.0.0.2: blue at 10.0.0.2:8080 greets 10.0.0.3\n\
             bind to 10.0.0.2: not available\n\
             loopback: refused\n\
             10.0.0.200: unreachable\n",
            "seed {seed}"
        );
        assert_eq!(r.u64("run.net_connections"), 4);
        assert_eq!(r.u64("run.net_passthrough"), 0);
        let again = run_manifest(&manifest, &scratch, seed, 3);
        assert_eq!(again.stdout, r.stdout);
        assert_eq!(
            r.fields["run.schedule_hash"],
            again.fields["run.schedule_hash"]
        );
    }
}

#[test]
fn timed_waits_follow_the_virtual_clock() {
    let dir = common::scratch_dir("multiproc_timers");
    common::build_c("timers", &dir, &[]);
    let manifest = dir.join("timers.manifest");
    std::fs::write(&manifest, "host a\n    timers\n").unwrap();
    let scratch = dir.join("scratch");
    let started = std::time::Instant::now();
    for seed in 1..=4u64 {
        let r = run_manifest(&manifest, &scratch, seed, 1);
        let out = &r.stdout[0];
        assert!(
            out.starts_with("sleepers woke in order 10 20 30\n"),
            "seed {seed}: {out}"
        );
        assert!(!out.contains("NO"), "seed {seed}: {out}");
        assert_eq!(out.lines().count(), 19, "seed {seed}: {out}");
    }
    // Virtual time: the waits add up to seconds of timeouts that nobody sat through
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
}
