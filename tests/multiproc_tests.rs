//! Multi-process acceptance: guests listed in a YAML run file run under one
//! scheduler, and the run-wide schedule is a function of the seed.

mod common;

use std::path::Path;
use std::process::Command;

use common::{run_manifest, run_manifest_with, RunReport};

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
        assert_eq!(lines[0], "parent pid=100000 ppid=1", "seed {seed}");
        assert!(
            lines.contains(&"spawned 100001 100002 100003 100004".to_string()),
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
            let expect = format!("child {i} pid={} ppid=100000 sum=", 100_001 + i);
            assert!(line.starts_with(&expect), "{line}");
        }
        let reaped: Vec<&String> = sorted.iter().filter(|l| l.starts_with("reaped")).collect();
        let expect: Vec<String> = (0..4)
            .map(|i| format!("reaped {} exit={}", 100_001 + i, 10 + i))
            .collect();
        assert_eq!(
            reaped.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            expect
        );
        // The first reap names a pid, so it is child 1 whatever the schedule
        let first = lines.iter().find(|l| l.starts_with("reaped")).unwrap();
        assert_eq!(first, "reaped 100002 exit=11");

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
        assert_eq!(r.stdout[0], "child of 100000 runs on red\n", "seed {seed}");
        assert_eq!(r.stdout[1], "child of 100001 runs on blue\n", "seed {seed}");
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
    // Builds and the first rewrite are not what is being timed
    run_manifest(&manifest, &scratch, 1, 1);
    let started = std::time::Instant::now();
    for seed in 1..=4u64 {
        let r = run_manifest(&manifest, &scratch, seed, 1);
        let out = &r.stdout[0];
        assert!(
            out.starts_with("sleepers woke in order 10 20 30\n"),
            "seed {seed}: {out}"
        );
        assert!(!out.contains("NO"), "seed {seed}: {out}");
        assert_eq!(out.lines().count(), 21, "seed {seed}: {out}");
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

    // The default scratch directory is under /var/folders, which is a system
    // location; a sibling host's directory must not pass as "system".
    let tmp = std::env::temp_dir().join(format!("rewrite-hostfs-test-{}", std::process::id()));
    let under_tmp = run_manifest(&manifest, &tmp, 1, 2);
    let _ = std::fs::remove_dir_all(&tmp);
    assert_eq!(under_tmp.stdout, r.stdout);
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

#[test]
fn a_guest_that_uses_gcd_is_turned_away() {
    let dir = common::scratch_dir("multiproc_gcd");
    common::build_c("gcd_user", &dir, &[]);
    let scratch = dir.join("scratch");
    common::supervisor_dylib();
    let run = |mode: &str| {
        let manifest = dir.join(format!("{mode}.yaml"));
        std::fs::write(
            &manifest,
            format!("hosts:\n  - name: a\n    processes:\n      - gcd_user {mode}\n"),
        )
        .unwrap();
        let out = Command::new(common::rewrite_bin())
            .args(["run", "--capture", "--seed", "1", "--scratch"])
            .arg(&scratch)
            .arg("--manifest")
            .arg(&manifest)
            .output()
            .unwrap();
        let stdout = std::fs::read_to_string(scratch.join("stdout.0")).unwrap();
        (
            out.status.code(),
            stdout,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let (code, stdout, stderr) = run("async");
    assert_eq!(code, Some(69), "{stderr}");
    assert_eq!(stdout, "about to use dispatch_async_f\n");
    assert!(
        stderr.contains("dispatch_async_f: Grand Central Dispatch is not supported"),
        "{stderr}"
    );
    // Work that stays on the calling thread is fine
    let (code, stdout, stderr) = run("sync");
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, "block ran: sync\ndone\n");
}

#[test]
fn guests_start_from_a_fixed_environment() {
    let dir = common::scratch_dir("multiproc_env");
    common::build_c("envprobe", &dir, &[]);
    let manifest = dir.join("env.yaml");
    std::fs::write(
        &manifest,
        r"env: { FROM_FILE: run-wide }
pass-env: [PASSED, NOT_SET_ANYWHERE]
hosts:
  - name: a
    processes:
      - argv: [envprobe]
        env: { MINE: just-me }
      - envprobe
",
    )
    .unwrap();
    let scratch = dir.join("scratch");
    common::supervisor_dylib();
    // Two shells that could hardly differ more
    let ambient: [&[(&str, String)]; 2] = [
        &[
            ("AMBIENT", "x".to_string()),
            ("PASSED", "on-purpose".to_string()),
        ],
        &[
            ("AMBIENT", "y".repeat(3000)),
            ("PASSED", "on-purpose".to_string()),
            ("OLDPWD", "/somewhere/else/entirely".to_string()),
            ("http_proxy", "http://proxy.invalid:3128".to_string()),
            ("LANG", "de_DE.UTF-8".to_string()),
            ("TZ", "Pacific/Auckland".to_string()),
        ],
    ];
    let mut seen = Vec::new();
    for (i, vars) in ambient.iter().enumerate() {
        let trace = dir.join(format!("trace{i}"));
        let _ = std::fs::remove_file(&trace);
        let out = Command::new(common::rewrite_bin())
            .args(["run", "--capture", "--seed", "3", "--scratch"])
            .arg(&scratch)
            .arg("--manifest")
            .arg(&manifest)
            .envs(vars.iter().map(|(k, v)| (k, v)))
            .env("REWRITE_TRACE", &trace)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let read = |n: usize| std::fs::read_to_string(scratch.join(format!("stdout.{n}"))).unwrap();
        seen.push((read(0), read(1), std::fs::read_to_string(&trace).unwrap()));
    }
    let (first, second, trace) = &seen[0];
    let lines: Vec<&str> = first.lines().collect();
    assert_eq!(
        lines[0],
        "FROM_FILE HOME LANG LC_ALL LOGNAME MINE PASSED PATH PWD TMPDIR TZ USER "
    );
    assert_eq!(
        lines[1],
        "PATH=/usr/bin:/bin:/usr/sbin:/sbin LANG=C TZ=UTC USER=guest"
    );
    assert_eq!(
        lines[2],
        "AMBIENT=(unset) PASSED=on-purpose FROM_FILE=run-wide MINE=just-me"
    );
    assert!(second.contains("MINE=(unset)"), "{second}");
    assert!(trace.lines().count() > 3, "{trace}");
    // Same output (stack address included) and the same trace, byte for
    // byte, whatever the launcher's own environment was
    assert_eq!(seen[0], seen[1]);
}

#[test]
fn a_deadlock_across_processes_ends_the_run() {
    let dir = common::scratch_dir("multiproc_deadlock");
    common::build_c("lifecycle", &dir, &[]);
    let manifest = dir.join("stuck.yaml");
    std::fs::write(
        &manifest,
        "hosts:\n  - name: a\n    processes:\n      - lifecycle stuck\n      - lifecycle stuck\n",
    )
    .unwrap();
    common::supervisor_dylib();
    let out = Command::new(common::rewrite_bin())
        .args(["run", "--capture", "--seed", "1", "--scratch"])
        .arg(dir.join("scratch"))
        .arg("--manifest")
        .arg(&manifest)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    // Whoever finds the run idle aborts; the launcher passes the baton on,
    // and the other guest finds the same. Before, the first abort left
    // nobody with the baton and the launcher waited forever.
    assert_eq!(
        err.matches("deadlock: every thread is blocked").count(),
        2,
        "{err}"
    );
    assert!(
        err.contains("p0.status=signal 6") && err.contains("p1.status=signal 6"),
        "{err}"
    );
}

#[test]
fn a_killed_child_can_be_reaped_at_once() {
    let dir = common::scratch_dir("multiproc_kill");
    common::build_c("lifecycle", &dir, &[]);
    let manifest = dir.join("kill.yaml");
    std::fs::write(
        &manifest,
        "hosts:\n  - name: a\n    processes:\n      - lifecycle killer\n",
    )
    .unwrap();
    let scratch = dir.join("scratch");
    for seed in 1..=3u64 {
        let r = run_manifest(&manifest, &scratch, seed, 1);
        assert_eq!(
            r.stdout[0], "killing 100001: 0\nreaped 100001, killed by a signal: yes\nagain: -1\n",
            "seed {seed}"
        );
        let again = run_manifest(&manifest, &scratch, seed, 1);
        assert_eq!(
            again.fields["run.schedule_hash"],
            r.fields["run.schedule_hash"]
        );
    }
}

#[test]
fn daemons_are_killed_in_runs_without_the_scheduler_too() {
    let dir = common::scratch_dir("multiproc_native_daemon");
    common::build_c("lifecycle", &dir, &[]);
    common::build_c("envprobe", &dir, &[]);
    let manifest = dir.join("daemon.yaml");
    std::fs::write(
        &manifest,
        "hosts:\n  - name: a\n    processes:\n      - argv: [lifecycle, forever]\n        daemon: true\n      - envprobe\n",
    )
    .unwrap();
    for mode in ["--native", "--no-supervisor"] {
        let out = Command::new(common::rewrite_bin())
            .args(["run", "--capture", mode, "--scratch"])
            .arg(dir.join("scratch"))
            .arg("--manifest")
            .arg(&manifest)
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{mode}: {err}");
        assert!(
            err.contains("p0.status=signal 9") && err.contains("p1.status=exit 0"),
            "{err}"
        );
    }
}

/// The indices of the `pong N life L` lines, and the lives that answered.
fn pongs(stdout: &str) -> (Vec<u32>, Vec<u32>) {
    stdout
        .lines()
        .filter_map(|l| {
            let mut words = l.strip_prefix("pong ")?.split(' ');
            let index: u32 = words.next()?.parse().ok()?;
            let life: u32 = words.nth(1)?.parse().ok()?;
            Some((index, life))
        })
        .unzip()
}

#[test]
fn a_server_that_keeps_crashing_is_restarted_and_the_client_reconnects() {
    let dir = common::scratch_dir("faults_server");
    common::build_c("ping_pong", &dir, &[]);
    let manifest = dir.join("server.yaml");
    std::fs::write(
        &manifest,
        r"hosts:
  - name: server
    processes:
      - argv: [ping_pong, pong, 7000]
        daemon: true
        restart: on-failure
        restart-delay: 50ms..150ms
        crash: { every: 100ms..300ms, times: 3 }
  - name: client
    processes:
      - ping_pong ping server 7000 120
",
    )
    .unwrap();
    let scratch = dir.join("scratch");
    let mut outages_by_seed = Vec::new();
    for seed in 1..=3u64 {
        let r = run_manifest(&manifest, &scratch, seed, 2);
        let (indices, lives) = pongs(&r.stdout[1]);
        // Every ping answered once, in order, by lives 1 to 4 in turn
        assert_eq!(indices, (0..120).collect::<Vec<u32>>(), "seed {seed}");
        assert!(
            lives.windows(2).all(|w| w[0] <= w[1]),
            "seed {seed}: {lives:?}"
        );
        assert_eq!((lives[0], lives[119]), (1, 4), "seed {seed}");
        assert!(r.stdout[1].ends_with("client life 1 done: 120 pongs, 3 reconnects\n"));
        assert_eq!(
            r.stdout[0],
            "server life 1 is up\nserver life 2 is up\nserver life 3 is up\nserver life 4 is up\n"
        );
        // A restart is not instantaneous: the server is down for its
        // restart delay, on the clock the client reads
        let outages: Vec<u64> = r.stdout[1]
            .lines()
            .filter_map(|l| {
                l.strip_prefix("server was out of reach for ")?
                    .strip_suffix(" ms")?
                    .parse()
                    .ok()
            })
            .collect();
        assert_eq!(outages.len(), 3, "seed {seed}");
        assert!(
            outages.iter().all(|&ms| (50..=200).contains(&ms)),
            "seed {seed}: {outages:?}"
        );
        assert_eq!(r.u64("run.crashes_injected"), 3);
        assert_eq!(r.u64("run.restarts"), 3);
        assert_eq!(r.u64("run.processes"), 5);
        assert_eq!(r.fields["p4.entry"], "0");
        // The host directory outlives the process
        assert_eq!(
            std::fs::read_to_string(scratch.join("server/lives")).unwrap(),
            "4\n"
        );

        let again = run_manifest(&manifest, &scratch, seed, 2);
        assert_eq!(again.stdout, r.stdout, "seed {seed} not repeatable");
        assert_eq!(
            again.fields["run.schedule_hash"],
            r.fields["run.schedule_hash"]
        );
        outages_by_seed.push(outages);
    }
    outages_by_seed.dedup();
    assert!(
        outages_by_seed.len() > 1,
        "every seed kept the server down for the same times"
    );
}

/// A crash wakes threads blocked on I/O. That must happen when the crash
/// does, and not again whenever the launcher gets to hear of the death: a
/// server idle in `accept` next to the crashing one would then run at
/// moments of real time.
#[test]
fn a_crash_disturbs_bystanders_at_a_fixed_point() {
    let dir = common::scratch_dir("faults_bystander");
    common::build_c("ping_pong", &dir, &[]);
    common::build_c("sleeper", &dir, &[]);
    let manifest = dir.join("bystander.yaml");
    std::fs::write(
        &manifest,
        "hosts:\n  - name: server\n    processes:\n      - argv: [ping_pong, pong, 7000]\n        daemon: true\n\
         \x20       restart: on-failure\n        restart-delay: 50ms..150ms\n\
         \x20       crash: { every: 100ms..300ms, times: 3 }\n\
         \x20 - name: bystander\n    processes:\n      - argv: [ping_pong, pong, 7001]\n        daemon: true\n\
         \x20 - name: client\n    processes:\n      - sleeper 1500 1000\n",
    )
    .unwrap();
    let scratch = dir.join("scratch");
    let first = run_manifest(&manifest, &scratch, 1, 3);
    assert_eq!(first.fields["run.crashes_injected"], "3");
    for _ in 0..7 {
        let again = run_manifest(&manifest, &scratch, 1, 3);
        assert_eq!(again.stdout, first.stdout);
        assert_eq!(
            again.fields["run.schedule_hash"],
            first.fields["run.schedule_hash"]
        );
    }
}

/// Any number of crashes may come due at the same moment of an idle run.
#[test]
fn many_processes_can_crash_at_once() {
    use std::fmt::Write;
    let dir = common::scratch_dir("faults_at_once");
    common::build_c("sleeper", &dir, &[]);
    common::supervisor_dylib();
    let mut text = String::from("hosts:\n  - name: h\n    processes:\n");
    for _ in 0..12 {
        write!(
            text,
            "      - argv: [sleeper, 3, 1000000]\n        crash: {{ every: 100ms }}\n"
        )
        .unwrap();
    }
    let manifest = dir.join("at_once.yaml");
    std::fs::write(&manifest, text).unwrap();
    let out = Command::new(common::rewrite_bin())
        .args(["run", "--capture", "--seed", "1", "--scratch"])
        .arg(dir.join("scratch"))
        .arg("--manifest")
        .arg(&manifest)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("run.crashes_injected=12\n"), "{err}");
}

#[test]
fn fault_settings_need_the_supervisor() {
    let dir = common::scratch_dir("faults_native");
    common::build_c("sleeper", &dir, &[]);
    let manifest = dir.join("native.yaml");
    std::fs::write(
        &manifest,
        "hosts:\n  - name: h\n    processes:\n      - argv: [sleeper, 1, 1]\n        restart: always\n",
    )
    .unwrap();
    for mode in ["--native", "--no-supervisor"] {
        let out = Command::new(common::rewrite_bin())
            .args(["run", mode, "--scratch"])
            .arg(dir.join("scratch"))
            .arg("--manifest")
            .arg(&manifest)
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{mode}");
        assert!(err.contains("need the supervisor"), "{mode}: {err}");
    }
}

#[test]
fn a_client_that_crashes_resumes_from_what_it_wrote_down() {
    let dir = common::scratch_dir("faults_client");
    common::build_c("ping_pong", &dir, &[]);
    let manifest = dir.join("client.yaml");
    std::fs::write(
        &manifest,
        r"hosts:
  - name: server
    processes:
      - argv: [ping_pong, pong, 7000]
        daemon: true
  - name: client
    processes:
      - argv: [ping_pong, ping, server, 7000, 120]
        restart: on-failure
        restart-delay: 200ms
        crash: { every: 150ms..250ms, times: 2 }
",
    )
    .unwrap();
    let scratch = dir.join("scratch");
    for seed in 1..=3u64 {
        let r = run_manifest(&manifest, &scratch, seed, 2);
        // Its three lives share one stdout. A pong may show twice if the
        // crash came between printing it and writing the progress down;
        // none may be missing.
        let (mut indices, _) = pongs(&r.stdout[1]);
        indices.dedup();
        assert_eq!(indices, (0..120).collect::<Vec<u32>>(), "seed {seed}");
        assert_eq!(
            r.stdout[1].matches(" resumes at ping ").count(),
            2,
            "seed {seed}"
        );
        assert!(r.stdout[1].ends_with("client life 3 done: 120 pongs, 0 reconnects\n"));
        assert_eq!(r.u64("run.crashes_injected"), 2);
        assert_eq!(r.u64("run.restarts"), 2);
        // The run's verdict is the last life's: it succeeded
        assert_eq!(r.fields["p1.status"], "signal 9");
        assert_eq!(r.fields["p3.status"], "exit 0");
        let again = run_manifest(&manifest, &scratch, seed, 2);
        assert_eq!(again.stdout, r.stdout, "seed {seed} not repeatable");
        assert_eq!(
            again.fields["run.schedule_hash"],
            r.fields["run.schedule_hash"]
        );
    }
}

#[test]
fn restarts_stop_where_the_run_file_says() {
    let dir = common::scratch_dir("faults_limits");
    common::build_c("ping_pong", &dir, &[]);
    common::supervisor_dylib();
    let scratch = dir.join("scratch");
    for (policy, crashes, restarts) in [
        ("restart: never", 1, 0),
        ("restart: on-failure\n        max-restarts: 1", 2, 1),
    ] {
        let manifest = dir.join("limits.yaml");
        std::fs::write(
            &manifest,
            format!(
                "hosts:\n  - name: server\n    processes:\n      - argv: [ping_pong, pong, 7000]\n        daemon: true\n\
                 \x20 - name: client\n    processes:\n      - argv: [ping_pong, ping, server, 7000, 100000]\n\
                 \x20       {policy}\n        crash: {{ every: 100ms }}\n"
            ),
        )
        .unwrap();
        let out = Command::new(common::rewrite_bin())
            .args(["run", "--capture", "--seed", "1", "--scratch"])
            .arg(&scratch)
            .arg("--manifest")
            .arg(&manifest)
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        // The client never finishes its 100,000 pings: its last life crashed
        assert!(!out.status.success(), "{policy}: {err}");
        assert!(
            err.contains(&format!("run.crashes_injected={crashes}\n")),
            "{policy}: {err}"
        );
        assert!(
            err.contains(&format!("run.restarts={restarts}\n")),
            "{policy}: {err}"
        );
    }
}

/// Guests must not outlive a launcher that is killed: one parked in
/// `accept` and one that keeps the baton busy both notice and exit.
#[test]
fn guests_exit_when_the_launcher_is_killed() {
    use std::time::{Duration, Instant};
    let dir = common::scratch_dir("orphans");
    common::build_c("ping_pong", &dir, &[]);
    common::build_c("lifecycle", &dir, &[]);
    common::supervisor_dylib();
    let manifest = dir.join("orphans.yaml");
    std::fs::write(
        &manifest,
        "hosts:\n  - name: a\n    processes:\n      - ping_pong pong 7000\n      - lifecycle forever\n",
    )
    .unwrap();
    let mut launcher = Command::new(common::rewrite_bin())
        .args(["run", "--capture", "--scratch"])
        .arg(dir.join("scratch"))
        .arg("--manifest")
        .arg(&manifest)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let children = || -> Vec<i32> {
        let out = Command::new("pgrep")
            .args(["-P", &launcher.id().to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    };
    let began = Instant::now();
    let mut guests = children();
    while guests.len() < 2 {
        assert!(
            began.elapsed() < Duration::from_secs(30),
            "guests never started"
        );
        std::thread::sleep(Duration::from_millis(50));
        guests = children();
    }
    // Let them attach and settle into the run
    std::thread::sleep(Duration::from_millis(500));
    launcher.kill().unwrap();
    launcher.wait().unwrap();

    let alive = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;
    let began = Instant::now();
    while guests.iter().any(|&g| alive(g)) {
        if began.elapsed() > Duration::from_secs(20) {
            for &g in &guests {
                unsafe { libc::kill(g, libc::SIGKILL) };
            }
            panic!("guests outlived the launcher");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn kevent_dispatch_registrations_fire_once_until_enabled() {
    let dir = common::scratch_dir("kq_dispatch");
    common::build_c("kq_dispatch", &dir, &[]);
    let manifest = dir.join("dispatch.yaml");
    std::fs::write(
        &manifest,
        "hosts:\n  - name: server\n    processes:\n      - kq_dispatch server 7000\n\
         \x20 - name: client\n    processes:\n      - kq_dispatch client server 7000\n",
    )
    .unwrap();
    let r = run_manifest(&manifest, &dir.join("scratch"), 1, 2);
    assert_eq!(
        r.stdout[0],
        "first wait: 1\nwhile disabled: 0\nenabled again: 1\nafter accept: 0\n"
    );
}

/// The kernel's SIGCHLD comes at a moment of real time on a thread of its
/// choosing; the guest's handler runs at a point of the schedule instead.
#[test]
fn sigchld_is_delivered_at_a_point_of_the_schedule() {
    let dir = common::scratch_dir("sigchld");
    common::build_c("sigchld", &dir, &[]);
    let manifest = dir.join("sigchld.yaml");
    std::fs::write(
        &manifest,
        "hosts:\n  - name: a\n    processes:\n      - sigchld\n",
    )
    .unwrap();
    let scratch = dir.join("scratch");
    let r = run_manifest(&manifest, &scratch, 1, 1);
    assert_eq!(
        r.stdout[0],
        "sigaction: exit 3, handler ran 1\nhandler reads back: yes\n\
         signal: exit 4, handler ran 1\ndefault: exit 5, handler ran 0\n"
    );
    let again = run_manifest(&manifest, &scratch, 1, 1);
    assert_eq!(
        again.fields["run.schedule_hash"],
        r.fields["run.schedule_hash"]
    );
}

/// Small programs from a review, each of which behaved differently under
/// the supervisor than natively: a kqueue holding a user event next to a
/// signal registration, a timer whose ident equals a closed descriptor, a
/// receipt call with no event list, `recv(MSG_DONTWAIT)` on a socket pair,
/// a `SIGCHLD` blocked around `fork`, the handler's `siginfo` and
/// `SA_RESETHAND`, and `kill(getpid())` with the signal blocked in the
/// caller. What they print natively is what they must print here.
#[test]
fn edge_cases_behave_as_they_do_natively() {
    let dir = common::scratch_dir("edges");
    for name in [
        "kquser",
        "kqtimer",
        "kqnull",
        "dontwait",
        "chldblock",
        "chldinfo",
        "selfkill",
    ] {
        let program = format!("edge_{name}");
        let exe = common::build_c(&program, &dir, &[]);
        let native = Command::new(&exe).output().unwrap();
        assert!(native.status.success(), "{name} natively");
        let manifest = dir.join(format!("{name}.yaml"));
        std::fs::write(
            &manifest,
            format!("hosts:\n  - name: a\n    processes:\n      - {program}\n"),
        )
        .unwrap();
        for seed in 1..=3 {
            let r = run_manifest(&manifest, &dir.join("scratch"), seed, 1);
            assert_eq!(
                r.stdout[0],
                String::from_utf8_lossy(&native.stdout),
                "{name}, seed {seed}"
            );
        }
    }
}

/// A guest's heap is 32 GB of address space unless the run says otherwise,
/// in the run file or on the command line, which wins.
#[test]
fn the_heap_size_is_the_runs_to_set() {
    let dir = common::scratch_dir("heap_size");
    common::build_c("heap_limit", &dir, &[]);
    let scratch = dir.join("scratch");
    let manifest = dir.join("heap.yaml");
    let processes = "hosts:\n  - name: a\n    processes:\n      - heap_limit\n";
    let (roomy, tight) = ("100 MB: ok\n1000 MB: ok\n", "100 MB: ok\n1000 MB: null\n");

    std::fs::write(&manifest, processes).unwrap();
    assert_eq!(run_manifest(&manifest, &scratch, 1, 1).stdout[0], roomy);
    let small = run_manifest_with(&manifest, &scratch, 1, 1, &["--heap-size", "256M"]);
    assert_eq!(small.stdout[0], tight);

    std::fs::write(&manifest, format!("heap-size: 256M\n{processes}")).unwrap();
    assert_eq!(run_manifest(&manifest, &scratch, 1, 1).stdout[0], tight);
    let large = run_manifest_with(&manifest, &scratch, 1, 1, &["--heap-size", "8G"]);
    assert_eq!(large.stdout[0], roomy);
}

/// A signal handler runs on a parked thread at a moment of real time and
/// may interrupt the thread inside the scheduler lock; it must not wait
/// for itself. And a thread killed by `execve` may be inside the lock: the
/// new image must not wait for a thread of the old one. Both guests hung
/// the run most of the time before the fixes.
#[test]
fn signal_handlers_and_execve_do_not_wedge_the_run() {
    let dir = common::scratch_dir("exec_race");
    for name in ["exec_race", "exec_race2"] {
        common::build_c(name, &dir, &[]);
        let manifest = dir.join(format!("{name}.yaml"));
        std::fs::write(
            &manifest,
            format!("hosts:\n  - name: a\n    processes:\n      - {name} 100\n"),
        )
        .unwrap();
        let scratch = dir.join("scratch");
        for seed in 1..=3 {
            let out = common::run_manifest_timed(
                &manifest,
                &scratch,
                seed,
                std::time::Duration::from_mins(1),
            )
            .unwrap_or_else(|| panic!("{name}, seed {seed}: the run hung"));
            let report = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "{name}, seed {seed}: {report}");
            assert_eq!(
                std::fs::read_to_string(scratch.join("stdout.0")).unwrap(),
                "done\n",
                "{name}, seed {seed}"
            );
        }
    }
}
