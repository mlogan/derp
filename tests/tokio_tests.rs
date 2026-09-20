//! A guest on an async runtime: tokio's worker threads, its kqueue reactor
//! and waker, its blocking pool and timers, and the `tokio::sync`
//! primitives, first in one process and then as a key-value server with
//! clients on other hosts. Everything must hold for every schedule, and a
//! seed must give the same schedule twice.

mod common;

use common::{run_manifest_with, RunReport};
use std::process::Command;

/// A small quantum: many more switch points inside the runtime's own code
/// than the default would give a run this short.
const SMALL_QUANTUM: [&str; 2] = ["--quantum", "50..500"];

fn twice(manifest: &std::path::Path, scratch: &std::path::Path, seed: u64, n: usize) -> RunReport {
    twice_with(manifest, scratch, seed, n, &[])
}

fn twice_with(
    manifest: &std::path::Path,
    scratch: &std::path::Path,
    seed: u64,
    n: usize,
    extra: &[&str],
) -> RunReport {
    let options = [&SMALL_QUANTUM[..], extra].concat();
    let first = run_manifest_with(manifest, scratch, seed, n, &options);
    let again = run_manifest_with(manifest, scratch, seed, n, &options);
    assert_eq!(again.stdout, first.stdout, "seed {seed} not repeatable");
    assert_eq!(
        again.fields["run.schedule_hash"], first.fields["run.schedule_hash"],
        "seed {seed} not repeatable"
    );
    first
}

#[test]
fn tokio_sync_primitives_hold_under_every_schedule() {
    let dir = common::scratch_dir("tokio_sync");
    let kv = common::build_kv(&dir);
    let native = Command::new(&kv).arg("sync").output().unwrap();
    assert!(native.status.success(), "{:?}", native.status);
    let expected = String::from_utf8(native.stdout).unwrap();
    assert!(expected.ends_with("sync ok\n"), "{expected}");

    let manifest = dir.join("sync.yaml");
    std::fs::write(
        &manifest,
        "hosts:\n  - name: a\n    processes:\n      - kv sync\n",
    )
    .unwrap();
    let mut hashes = Vec::new();
    for (seed, hooks) in [(1, "0"), (2, "0"), (3, "1/16"), (4, "1")] {
        let r = twice_with(
            &manifest,
            &dir.join("scratch"),
            seed,
            1,
            &["--mem-hook-rate", hooks],
        );
        // The program checks itself; what it prints is the same always
        assert_eq!(r.stdout[0], expected, "seed {seed}");
        assert!(r.u64("p0.threads") >= 5, "workers and a blocking pool");
        hashes.push(r.fields["run.schedule_hash"].clone());
    }
    hashes.sort();
    hashes.dedup();
    assert_eq!(hashes.len(), 4, "seeds should schedule differently");
}

const KV_RUN: &str = "hosts:
  - name: server
    processes:
      - argv: [kv, server, 7000, 2, kv.log]
SERVER_EXTRA  - name: c0
    processes:
      - kv client server 7000 0 20
  - name: c1
    processes:
      - kv client server 7000 1 20
";

/// Three lines of one client's summary: counter, reconnects, changes seen.
fn client_line(line: &str, id: u32) -> (u32, u32) {
    let rest = line
        .strip_prefix(&format!("client {id}: VALUE 60 reconnects="))
        .unwrap_or_else(|| panic!("{line}"));
    let (reconnects, changes) = rest.trim_end().split_once(" changes=").unwrap();
    (reconnects.parse().unwrap(), changes.parse().unwrap())
}

#[test]
fn tokio_kv_server_and_clients_over_the_virtual_network() {
    let dir = common::scratch_dir("tokio_kv");
    common::build_kv(&dir);
    let manifest = dir.join("kv.yaml");
    std::fs::write(&manifest, KV_RUN.replace("SERVER_EXTRA", "")).unwrap();
    for seed in 1..=3 {
        let r = twice(&manifest, &dir.join("scratch"), seed, 3);
        // 2 clients x 3 tasks x 20 rounds, each adding to `total` once
        assert!(
            r.stdout[0].starts_with("server: total=120 done=2 Stats { sets: 120,"),
            "seed {seed}: {}",
            r.stdout[0]
        );
        for id in 0..2 {
            let (reconnects, changes) = client_line(&r.stdout[id as usize + 1], id);
            assert_eq!(reconnects, 0);
            assert!(changes > 100, "the subscription saw {changes} changes");
        }
        // Per client: three workers, a subscriber and the last check
        assert_eq!(r.u64("run.net_connections"), 10);
    }
}

/// The server writes every change down before it answers, so crashing it
/// loses nothing: clients reconnect, resend, and the total is still exact
/// because a resent `INCR` is recognised.
#[test]
fn tokio_kv_server_survives_crashes() {
    let dir = common::scratch_dir("tokio_kv_crash");
    common::build_kv(&dir);
    let manifest = dir.join("crash.yaml");
    let faults = "        restart: on-failure\n        restart-delay: 50ms..150ms\n\
                  \x20       crash: { every: 150ms..400ms, times: 3 }\n";
    std::fs::write(&manifest, KV_RUN.replace("SERVER_EXTRA", faults)).unwrap();
    // Also with switch points at memory accesses: between the halves of the
    // runtime's own atomics-and-queues protocols, and with less done per
    // virtual millisecond, so crashes land earlier in the work
    for (seed, hooks) in [(1, "0"), (2, "0"), (2, "1/4")] {
        let r = twice_with(
            &manifest,
            &dir.join("scratch"),
            seed,
            3,
            &["--mem-hook-rate", hooks],
        );
        assert_eq!(r.u64("run.crashes_injected"), 3, "seed {seed}");
        assert_eq!(r.u64("run.restarts"), 3, "seed {seed}");
        // Only the last life gets to print
        assert!(
            r.stdout[0].starts_with("server: total=120 done=2 "),
            "seed {seed}: {}",
            r.stdout[0]
        );
        let reconnects: u32 = (0..2)
            .map(|id| client_line(&r.stdout[id as usize + 1], id).0)
            .sum();
        assert!(reconnects >= 3, "seed {seed}: {reconnects} reconnects");
    }
}

/// The same primitives with every task on one thread, and the parts of
/// tokio that lean on the operating system: files through the blocking
/// pool, signals through the self-pipe, child processes through SIGCHLD.
#[test]
fn tokio_current_thread_runtime_and_os_facing_parts() {
    let dir = common::scratch_dir("tokio_extras");
    let kv = common::build_kv(&dir);
    for mode in ["sync current", "extras"] {
        let native = Command::new(&kv)
            .args(mode.split(' '))
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(native.status.success(), "{mode}: {:?}", native.status);
        let expected = String::from_utf8(native.stdout).unwrap();
        let manifest = dir.join("mode.yaml");
        std::fs::write(
            &manifest,
            format!("hosts:\n  - name: a\n    processes:\n      - kv {mode}\n"),
        )
        .unwrap();
        for seed in 1..=3 {
            // `extras` starts three children
            let r = twice(&manifest, &dir.join("scratch"), seed, 1);
            assert_eq!(r.stdout[0], expected, "{mode}, seed {seed}");
        }
    }
}

/// Busy threads in this process while it lives: real time then treats the
/// guests differently from run to run, which is what shakes out anything
/// that still depends on it.
struct CpuLoad {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl CpuLoad {
    fn start(n: usize) -> CpuLoad {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let threads = (0..n)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                })
            })
            .collect();
        CpuLoad { stop, threads }
    }
}

impl Drop for CpuLoad {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// `kv addrs` prints where the heap would put a block of each size class at
/// every step of work that starts and ends threads, runs the blocking pool,
/// takes signals and reaps children. Its output is therefore a record of the
/// heap's whole history: one allocation or free that came at a moment of
/// real time moves some address after it. The schedule hash alone can miss
/// that (it only sees the addresses of contended locks).
///
/// `REWRITE_STRESS_RUNS=2000 cargo test heap_addresses` hunts for rare cases.
#[test]
fn tokio_heap_addresses_are_a_function_of_the_seed() {
    let dir = common::scratch_dir("tokio_addrs");
    common::build_kv(&dir);
    let manifest = dir.join("addrs.yaml");
    std::fs::write(
        &manifest,
        "hosts:\n  - name: a\n    processes:\n      - kv addrs\n",
    )
    .unwrap();
    let runs: usize = std::env::var("REWRITE_STRESS_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let _load = CpuLoad::start(2 * std::thread::available_parallelism().map_or(8, usize::from));
    for seed in [1, 2] {
        let first = run_manifest_with(&manifest, &dir.join("scratch"), seed, 1, &SMALL_QUANTUM);
        assert!(first.stdout[0].contains("heap end:"));
        for run in 1..runs {
            let again = run_manifest_with(&manifest, &dir.join("scratch"), seed, 1, &SMALL_QUANTUM);
            if again.stdout[0] != first.stdout[0] {
                let moved = first.stdout[0]
                    .lines()
                    .zip(again.stdout[0].lines())
                    .find(|(a, b)| a != b)
                    .map_or_else(
                        || "(length)".to_string(),
                        |(a, b)| format!("\n  {a}\n  {b}"),
                    );
                let kept = dir.join(format!("diverged.seed{seed}.run{run}"));
                std::fs::write(
                    &kept,
                    format!("{}\n=====\n{}", first.stdout[0], again.stdout[0]),
                )
                .unwrap();
                panic!(
                    "seed {seed}, run {run}: the heap went differently; first difference:{moved}\n\
                     both outputs kept in {}",
                    kept.display()
                );
            }
            assert_eq!(
                again.fields["run.schedule_hash"], first.fields["run.schedule_hash"],
                "seed {seed}, run {run}: same heap, different schedule"
            );
        }
    }
}
