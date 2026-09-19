//! Multi-process acceptance: guests listed in a YAML run file run under one
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

/// One `rewrite run --capture`: the guests' stdout from the scratch
/// directory and the aggregated report from the launcher's stderr.
fn run_manifest(manifest: &Path, scratch: &Path, seed: u64, guests: usize) -> RunReport {
    run_manifest_with(manifest, scratch, seed, guests, &[])
}

fn run_manifest_with(
    manifest: &Path,
    scratch: &Path,
    seed: u64,
    guests: usize,
    extra: &[&str],
) -> RunReport {
    common::supervisor_dylib();
    let report = Command::new(common::rewrite_bin())
        .args(["run", "--capture", "--seed", &seed.to_string()])
        .args(extra)
        .arg("--scratch")
        .arg(scratch)
        .arg("--manifest")
        .arg(manifest)
        .output()
        .expect("rewrite run");
    assert!(
        report.status.success(),
        "seed {seed}: {}",
        String::from_utf8_lossy(&report.stderr)
    );
    let stdout = (0..guests)
        .map(|i| std::fs::read_to_string(scratch.join(format!("stdout.{i}"))).unwrap())
        .collect();
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
    std::fs::write(
        &manifest,
        r"hosts:
  - name: a
    processes:
      - loops 1
  - name: b
    processes:
      - loops 1
",
    )
    .unwrap();
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
    std::fs::write(
        &manifest,
        r"hosts:
  - name: a
    processes:
      - crash
      - fine
",
    )
    .unwrap();
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
    std::fs::write(
        &manifest,
        r"hosts:
  - name: a
    processes:
      - spawn_tree
",
    )
    .unwrap();
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
    std::fs::write(
        &manifest,
        r"hosts:
  - name: a
    processes:
      - pipeline
",
    )
    .unwrap();
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
        r"hosts:
  - name: alpha
    processes:
      - tcp_echo server 7000 4
  - name: beta
    processes:
      - tcp_echo client alpha 7000 4
",
    );
}

#[test]
fn unix_domain_echo_on_one_host() {
    check_echo(
        "multiproc_unix",
        r"hosts:
  - name: alpha
    processes:
      - tcp_echo server --unix /virtual/echo.sock 4
      - tcp_echo client --unix /virtual/echo.sock 4
",
    );
}

#[test]
fn udp_ping_survives_lost_datagrams() {
    let dir = common::scratch_dir("multiproc_udp");
    common::build_c("udp_ping", &dir, &[]);
    let manifest = dir.join("udp.manifest");
    std::fs::write(
        &manifest,
        r"hosts:
  - name: alpha
    processes:
      - udp_ping server 5353
  - name: beta
    processes:
      - udp_ping client alpha 5353 20
",
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
        r"hosts:
  - name: red
    processes:
      - two_hosts server 8080
  - name: blue
    processes:
      - two_hosts server 8080
  - name: green
    processes:
      - two_hosts client 8080 red blue
",
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
    std::fs::write(
        &manifest,
        r"hosts:
  - name: a
    processes:
      - timers
",
    )
    .unwrap();
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

#[test]
fn a_fixed_latency_between_hosts_changes_the_schedule_not_the_results() {
    let dir = common::scratch_dir("multiproc_latency");
    common::build_c("tcp_echo", &dir, &[]);
    common::build_c("udp_ping", &dir, &[]);
    let scratch = dir.join("scratch");
    let echo = dir.join("echo.manifest");
    std::fs::write(
        &echo,
        r"hosts:
  - name: alpha
    processes:
      - tcp_echo server 7000 4
  - name: beta
    processes:
      - tcp_echo client alpha 7000 4
",
    )
    .unwrap();
    let udp = dir.join("udp.manifest");
    std::fs::write(
        &udp,
        r"hosts:
  - name: alpha
    processes:
      - udp_ping server 5353
  - name: beta
    processes:
      - udp_ping client alpha 5353 20
",
    )
    .unwrap();
    let latency = ["--net-latency", "5ms"];
    for seed in 1..=3u64 {
        let plain = run_manifest(&echo, &scratch, seed, 2);
        let slow = run_manifest_with(&echo, &scratch, seed, 2, &latency);
        assert_eq!(slow.stdout[0], "server done after 4 connections\n");
        assert_eq!(mask_blob(&slow.stdout[1]), expected_echo(4), "seed {seed}");
        assert_eq!(slow.stdout[1], plain.stdout[1], "seed {seed}");
        assert_ne!(
            slow.fields["run.schedule_hash"], plain.fields["run.schedule_hash"],
            "seed {seed}: latency did not change the schedule"
        );
        let again = run_manifest_with(&echo, &scratch, seed, 2, &latency);
        assert_eq!(again.stdout, slow.stdout);
        assert_eq!(
            again.fields["run.schedule_hash"],
            slow.fields["run.schedule_hash"]
        );

        let pings = run_manifest_with(&udp, &scratch, seed, 2, &latency);
        let pongs: Vec<String> = (0..20).map(|i| format!("pong {i}")).collect();
        assert_eq!(
            pings.stdout[1].lines().collect::<Vec<_>>(),
            pongs,
            "seed {seed}"
        );
        let again = run_manifest_with(&udp, &scratch, seed, 2, &latency);
        assert_eq!(again.stdout, pings.stdout);
        assert_eq!(
            again.fields["run.schedule_hash"],
            pings.fields["run.schedule_hash"]
        );
    }
}

fn check_poll_server(mode: &str) {
    let dir = common::scratch_dir(&format!("multiproc_{mode}"));
    common::build_c("poll_server", &dir, &[]);
    let manifest = dir.join("server.manifest");
    std::fs::write(
        &manifest,
        format!(
            r"hosts:
  - name: hub
    processes:
      - poll_server server {mode} 6000 4
  - name: spoke
    processes:
      - poll_server client hub 6000 0 5
      - poll_server client hub 6000 1 5
      - poll_server client hub 6000 2 5
      - poll_server client hub 6000 3 5 stall
"
        ),
    )
    .unwrap();
    let scratch = dir.join("scratch");
    let mut hashes = Vec::new();
    for seed in 1..=4u64 {
        let r = run_manifest(&manifest, &scratch, seed, 5);
        assert_eq!(
            r.stdout[0],
            format!(
                "client 3 timed out after 1 messages\n{mode} server: 4 clients, 16 messages, 1 timed out\n"
            ),
            "seed {seed}"
        );
        for id in 0..3 {
            let expect = (0..5).fold(String::new(), |mut all, m| {
                use std::fmt::Write;
                writeln!(all, "C{id} MESSAGE {m}").unwrap();
                all
            });
            assert_eq!(r.stdout[id + 1], expect, "seed {seed} client {id}");
        }
        assert_eq!(
            r.stdout[4], "C3 MESSAGE 0\nclient 3 dropped by the server before message 1\n",
            "seed {seed}"
        );
        assert_eq!(r.u64("run.net_passthrough"), 0);
        let again = run_manifest(&manifest, &scratch, seed, 5);
        assert_eq!(again.stdout, r.stdout);
        assert_eq!(
            r.fields["run.schedule_hash"], again.fields["run.schedule_hash"],
            "seed {seed} not repeatable"
        );
        hashes.push(r.fields["run.schedule_hash"].clone());
    }
    hashes.dedup();
    assert!(hashes.len() > 1, "every seed produced the same schedule");
}

#[test]
fn poll_server_multiplexes_clients_and_times_one_out() {
    check_poll_server("poll");
}

#[test]
fn kevent_server_multiplexes_clients_and_times_one_out() {
    check_poll_server("kevent");
}

#[test]
fn a_locked_file_counter_is_always_exact() {
    let dir = common::scratch_dir("multiproc_flock");
    common::build_c("counter_file", &dir, &[]);
    let manifest = dir.join("flock.manifest");
    std::fs::write(
        &manifest,
        r"hosts:
  - name: a
    processes:
      - counter_file counter.txt 4 300 --flock
",
    )
    .unwrap();
    let scratch = dir.join("scratch");
    let mut waited = 0;
    for seed in 1..=8u64 {
        let r = run_manifest(&manifest, &scratch, seed, 1);
        assert_eq!(r.stdout[0], "total=1200 expected=1200\n", "seed {seed}");
        waited += (1..=4)
            .map(|p| r.u64(&format!("p{p}.io_waits")))
            .sum::<u64>();
    }
    assert!(waited > 0, "no worker ever had to wait for the lock");
}

/// First seed in `seeds` whose run prints something other than `exact`.
fn find_lost_update(
    manifest: &Path,
    scratch: &Path,
    seeds: std::ops::RangeInclusive<u64>,
    extra: &[&str],
    exact: &str,
) -> (u64, RunReport) {
    for seed in seeds {
        let r = run_manifest_with(manifest, scratch, seed, 1, extra);
        assert!(
            r.stdout[0].starts_with("total="),
            "seed {seed}: {}",
            r.stdout[0]
        );
        if r.stdout[0] != exact {
            return (seed, r);
        }
    }
    panic!("no seed lost an update");
}

#[test]
fn an_unlocked_file_counter_loses_updates_reproducibly() {
    let dir = common::scratch_dir("multiproc_counter");
    common::build_c("counter_file", &dir, &[]);
    let manifest = dir.join("racy.manifest");
    std::fs::write(
        &manifest,
        r"hosts:
  - name: a
    processes:
      - counter_file counter.txt 4 500
",
    )
    .unwrap();
    let scratch = dir.join("scratch");
    // Branch hooks only: the switch between the read and the write comes
    // from the I/O calls being hook events.
    let (seed, lost) = find_lost_update(
        &manifest,
        &scratch,
        1..=20,
        &[],
        "total=2000 expected=2000\n",
    );
    for _ in 0..5 {
        let again = run_manifest(&manifest, &scratch, seed, 1);
        assert_eq!(again.stdout, lost.stdout, "seed {seed} not repeatable");
        assert_eq!(
            again.fields["run.schedule_hash"],
            lost.fields["run.schedule_hash"]
        );
    }
}

#[test]
fn a_shared_mapping_races_only_with_memory_hooks() {
    let dir = common::scratch_dir("multiproc_map");
    common::build_c("shared_map", &dir, &[]);
    let manifest = dir.join("map.manifest");
    std::fs::write(
        &manifest,
        r"hosts:
  - name: a
    processes:
      - shared_map
",
    )
    .unwrap();
    let scratch = dir.join("scratch");
    let exact = "total=400000 expected=400000\n";
    for seed in 1..=4u64 {
        let r = run_manifest(&manifest, &scratch, seed, 1);
        assert_eq!(r.stdout[0], exact, "branch hooks only, seed {seed}");
    }
    let sparse = ["--mem-hook-rate", "1/16"];
    let (seed, lost) = find_lost_update(&manifest, &scratch, 1..=40, &sparse, exact);
    for _ in 0..5 {
        let again = run_manifest_with(&manifest, &scratch, seed, 1, &sparse);
        assert_eq!(again.stdout, lost.stdout, "seed {seed} not repeatable");
        assert_eq!(
            again.fields["run.schedule_hash"],
            lost.fields["run.schedule_hash"]
        );
    }
}

#[test]
fn rust_std_net_echo_with_a_spawned_client() {
    let dir = common::scratch_dir("multiproc_rustnet");
    common::build_rust("net", &dir);
    let manifest = dir.join("net.manifest");
    std::fs::write(
        &manifest,
        r"hosts:
  - name: a
    processes:
      - net
",
    )
    .unwrap();
    let scratch = dir.join("scratch");
    let mut hashes = Vec::new();
    for seed in 1..=3u64 {
        let r = run_manifest_with(&manifest, &scratch, seed, 1, &["--mem-hook-rate", "1/16"]);
        let mut lines: Vec<&str> = r.stdout[0].lines().collect();
        lines.sort_unstable();
        let mut expect = vec![
            "client got: goodbye after 5 lines".to_string(),
            "server accepted a connection from 127.0.0.1".to_string(),
            "server saw 5 lines; client exited with exit status: 0".to_string(),
        ];
        expect.extend((0..5).map(|i| format!("client got: MESSAGE {i} FROM THE CLIENT")));
        expect.sort_unstable();
        assert_eq!(lines, expect, "seed {seed}");
        assert_eq!(r.u64("run.net_connections"), 1);
        assert_eq!(r.u64("run.net_passthrough"), 0);
        let again = run_manifest_with(&manifest, &scratch, seed, 1, &["--mem-hook-rate", "1/16"]);
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
fn run_file_settings_environment_and_errors() {
    let dir = common::scratch_dir("multiproc_runfile");
    common::build_c_source(
        "showenv",
        "#include <stdio.h>\n#include <stdlib.h>\nint main(int c, char **v){ const char *m = getenv(\"MODE\"); \
         printf(\"%s %s mode=%s\\n\", v[0], c > 1 ? v[1] : \"-\", m ? m : \"unset\"); return 0; }\n",
        &dir,
    );
    let manifest = dir.join("run.yaml");
    std::fs::write(
        &manifest,
        r#"seed: 41
quantum: 2000..3000
net-latency: 2ms
hosts:
  - name: a
    processes:
      - argv: [showenv, "two words"]
        env: { MODE: fast }
      - showenv plain
"#,
    )
    .unwrap();
    let scratch = dir.join("scratch");
    common::supervisor_dylib();
    let run = |extra: &[&str]| {
        let out = Command::new(common::rewrite_bin())
            .args(["run", "--capture"])
            .args(extra)
            .arg("--scratch")
            .arg(&scratch)
            .arg("--manifest")
            .arg(&manifest)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    // The seed is stamped into the rewritten binary, so the report shows
    // which one the run used.
    let report = run(&[]);
    assert!(report.contains("p0.seed=41"), "{report}");
    let read = |i: usize| std::fs::read_to_string(scratch.join(format!("stdout.{i}"))).unwrap();
    assert_eq!(read(0), "showenv two words mode=fast\n");
    assert_eq!(read(1), "showenv plain mode=unset\n");
    let report = run(&["--seed", "5"]);
    assert!(report.contains("p0.seed=5"), "{report}");

    for (text, complaint) in [
        (
            "hosts:\n  - name: a\n    root: /tmp/x\n    processes: [p]\n",
            "root",
        ),
        (
            "quantum: backwards\nhosts:\n  - name: a\n    processes: [showenv]\n",
            "bad quantum",
        ),
        ("host a\n    showenv\n", "run file"),
    ] {
        let bad = dir.join("bad.yaml");
        std::fs::write(&bad, text).unwrap();
        let out = Command::new(common::rewrite_bin())
            .args(["run", "--scratch"])
            .arg(&scratch)
            .arg("--manifest")
            .arg(&bad)
            .output()
            .unwrap();
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(complaint), "{complaint}: {err}");
    }
}

#[test]
fn each_host_is_held_to_its_own_directory() {
    let dir = common::scratch_dir("multiproc_hostfs");
    common::build_c("hostfs", &dir, &[]);
    std::fs::write(dir.join("seed.txt"), "copied in for red\n").unwrap();
    let manifest = dir.join("hostfs.yaml");
    std::fs::write(
        &manifest,
        r"hosts:
  - name: red
    files: [seed.txt]
    processes:
      - hostfs blue
  - name: blue
    processes:
      - hostfs red
",
    )
    .unwrap();
    let scratch = dir.join("scratch");
    // A leftover from an earlier run must not survive into the next one
    let r = run_manifest(&manifest, &scratch, 1, 2);
    std::fs::write(scratch.join("red/stale.txt"), "old").unwrap();
    let r2 = run_manifest(&manifest, &scratch, 1, 2);
    assert_eq!(r.stdout, r2.stdout);
    assert!(!scratch.join("red/stale.txt").exists());

    let expect = |me: &str, seed: &str| {
        format!(
            "{me} starts in a directory named {me}\n\
             HOME and PWD agree with it: yes\n\
             TMPDIR is inside it: yes\n\
             write data.txt: ok\n\
             seed.txt from the run file: {seed}\n\
             mkdir sub and a file by absolute path: ok\n\
             absolute path inside: ok\n\
             /etc/hosts: ok\n\
             /dev/null for writing: ok\n\
             stat /: ok\n\
             stat the directory above: ok\n\
             but not open it: refused\n\
             other host by relative path: refused\n\
             other host by absolute path: refused\n\
             stat the other host's file: No such file or directory\n\
             rename into the other host: refused\n\
             chdir to the other host: refused\n\
             a file in the real /tmp: refused\n\
             {me} starts in a directory named {me}\n\
             HOME and PWD agree with it: yes\n\
             TMPDIR is inside it: yes\n\
             child reads the other host's file: refused\n\
             child reads its own host's file: ok\n"
        )
    };
    assert_eq!(r.stdout[0], expect("red", "copied in for red"));
    assert_eq!(r.stdout[1], expect("blue", "(not copied)"));
    // The same relative name is a different file on each host
    let read = |host: &str| std::fs::read_to_string(scratch.join(host).join("data.txt")).unwrap();
    assert_eq!(read("red"), "written on red\n");
    assert_eq!(read("blue"), "written on blue\n");
    assert!(scratch.join("red/sub/inner.txt").exists());
    assert!(!Path::new("/tmp/rewrite-hostfs-escape").exists());
    assert_eq!(r.u64("p0.paths_refused"), 7);
    assert_eq!(r.u64("p2.paths_refused"), 1);
}

/// Homebrew's curl and python3.13, if installed. Apple's own copies ignore
/// `DYLD_INSERT_LIBRARIES`, so they cannot be guests.
fn homebrew_programs() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let curl = Path::new("/opt/homebrew/opt/curl/bin/curl");
    let python = std::fs::canonicalize("/opt/homebrew/opt/python@3.13/bin/python3.13").ok()?;
    curl.exists().then(|| (curl.to_path_buf(), python))
}

#[test]
fn curl_fetches_pages_from_python_web_servers_on_two_hosts() {
    let Some((curl, python)) = homebrew_programs() else {
        eprintln!("skipped: needs Homebrew's curl and python@3.13");
        return;
    };
    let dir = common::scratch_dir("multiproc_web");
    for host in ["alpha", "beta"] {
        let site = dir.join(host).join("site");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(
            site.join("index.html"),
            format!("<h1>hello from {host}</h1>\n"),
        )
        .unwrap();
    }
    let server = format!(
        "argv: [{}, -u, -m, http.server, 8000, --bind, 0.0.0.0, --directory, site]\n        daemon: true",
        python.display()
    );
    let manifest = dir.join("web.yaml");
    std::fs::write(
        &manifest,
        format!(
            "hosts:\n  - name: alpha\n    files: [alpha/site]\n    processes:\n      - {server}\n\
             \x20 - name: beta\n    files: [beta/site]\n    processes:\n      - {server}\n\
             \x20 - name: client\n    processes:\n\
             \x20     - [{}, -sS, --retry, 50, --retry-connrefused, --retry-delay, 1,\n\
             \x20        \"http://alpha:8000/index.html\", \"http://beta:8000/index.html\"]\n",
            curl.display()
        ),
    )
    .unwrap();
    let scratch = dir.join("scratch");
    let mut hashes = Vec::new();
    for seed in 1..=2u64 {
        let r = run_manifest(&manifest, &scratch, seed, 3);
        assert_eq!(
            r.stdout[2], "<h1>hello from alpha</h1>\n<h1>hello from beta</h1>\n",
            "seed {seed}"
        );
        assert!(
            r.stdout[0].starts_with("Serving HTTP on 0.0.0.0 port 8000"),
            "{}",
            r.stdout[0]
        );
        // Daemons are killed when curl is done; nothing left the virtual network
        assert_eq!(r.fields["p0.status"], "signal 9");
        assert_eq!(r.fields["p2.status"], "exit 0");
        assert_eq!(r.u64("run.net_connections"), 2);
        assert_eq!(r.u64("run.net_passthrough"), 0);
        for _ in 0..3 {
            let again = run_manifest(&manifest, &scratch, seed, 3);
            assert_eq!(again.stdout, r.stdout, "seed {seed} not repeatable");
            assert_eq!(
                again.fields["run.schedule_hash"],
                r.fields["run.schedule_hash"]
            );
        }
        hashes.push(r.fields["run.schedule_hash"].clone());
    }
    hashes.dedup();
    assert!(hashes.len() > 1, "every seed produced the same schedule");
}
