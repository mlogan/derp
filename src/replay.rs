//! Running one run file many times over, for the tools that look into a
//! failing seed (`bisect`, `suspects`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct Replay {
    pub manifest: PathBuf,
    /// The tool's own directory under the scratch directory
    pub dir: PathBuf,
    /// Every option of the run, the seed included, spelled out: what is
    /// given on the command line is not the run file's to change
    pub pass: Vec<String>,
    /// A run that takes longer is killed and counts as having ended
    /// differently. Set from the reference run.
    pub timeout: Duration,
}

/// What a run's report says about how it ended
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ending {
    /// The run-file entry that failed the run and its status
    pub failure: Option<(usize, String)>,
    pub failure_at: u64,
    pub schedule_hash: String,
    /// The status of each entry's last life
    pub last_status: BTreeMap<usize, String>,
    /// Which entry each process was a life of, by `p<N>`
    pub entry_of: BTreeMap<String, usize>,
    /// What each process ran, by `p<N>`: the original program, and the
    /// executable made from it
    pub program_of: BTreeMap<String, (PathBuf, PathBuf)>,
    pub timed_out: bool,
}

impl Ending {
    #[must_use]
    pub fn parse(stderr: &str) -> Ending {
        let mut e = Ending::default();
        let mut status_of: BTreeMap<String, String> = BTreeMap::new();
        for line in stderr.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "run.failure" => {
                    e.failure = value
                        .strip_prefix("entry ")
                        .and_then(|v| v.split_once(": "))
                        .and_then(|(n, status)| Some((n.parse().ok()?, status.to_string())));
                }
                "run.failure_at" => e.failure_at = value.parse().unwrap_or(0),
                "run.schedule_hash" => e.schedule_hash = value.to_string(),
                _ => {
                    if let Some(process) = key.strip_suffix(".status") {
                        status_of.insert(process.to_string(), value.to_string());
                    } else if let Some(process) = key.strip_suffix(".entry") {
                        if let Ok(entry) = value.parse() {
                            e.entry_of.insert(process.to_string(), entry);
                        }
                    } else if let Some(process) = key.strip_suffix(".program") {
                        let entry = e.program_of.entry(process.to_string()).or_default();
                        entry.0 = PathBuf::from(value);
                    } else if let Some(process) = key.strip_suffix(".image") {
                        let entry = e.program_of.entry(process.to_string()).or_default();
                        entry.1 = PathBuf::from(value);
                    }
                }
            }
        }
        // Processes are numbered in order of creation: the highest of an
        // entry is its last life
        let mut lives: Vec<(usize, &String, usize)> = e
            .entry_of
            .iter()
            .filter_map(|(p, &entry)| Some((p.strip_prefix('p')?.parse().ok()?, p, entry)))
            .collect();
        lives.sort_unstable();
        for (_, process, entry) in lives {
            if let Some(status) = status_of.get(process) {
                e.last_status.insert(entry, status.clone());
            }
        }
        e
    }

    /// Whether this run failed as `reference` did: the same entry ended with
    /// the same status, whatever else went wrong besides.
    #[must_use]
    pub fn fails_like(&self, reference: &Ending) -> bool {
        reference
            .failure
            .as_ref()
            .is_some_and(|(entry, status)| self.last_status.get(entry) == Some(status))
    }
}

impl Replay {
    /// Where slot `slot`'s schedule trace goes. Slots are two digits wide so
    /// that every run's paths are as long: a guest's environment holds its
    /// host directory, and the environment's size places its stack.
    #[must_use]
    pub fn trace_path(&self, slot: u32) -> PathBuf {
        self.dir.join(format!("trace.{slot:02}"))
    }

    /// One run in slot `slot` (its own scratch directory, trace and mask).
    /// The trace is always written and the mask always given, so that no
    /// run differs from another in anything but `extra` and `mask`.
    pub fn run(&self, slot: u32, extra: &[String], mask: &str) -> Ending {
        let mask_path = self.dir.join(format!("mask.{slot:02}"));
        std::fs::write(&mask_path, mask).expect("writing the mask");
        let trace = self.trace_path(slot);
        // The supervisor appends
        let _ = std::fs::remove_file(&trace);
        let mut child = Command::new(std::env::current_exe().expect("own path"))
            .args(["run", "--capture"])
            .args(&self.pass)
            .args(extra)
            .arg("--scratch")
            .arg(self.dir.join(format!("slot{slot:02}")))
            .arg("--manifest")
            .arg(&self.manifest)
            .env("REWRITE_MASK", &mask_path)
            .env("REWRITE_TRACE", &trace)
            .env("REWRITE_EXIT_WITH_PARENT", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("running rewrite");
        let mut stderr = child.stderr.take().expect("piped");
        let reader = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut stderr, &mut text);
            text
        });
        let began = Instant::now();
        let mut timed_out = false;
        while child.try_wait().ok().flatten().is_none() {
            if began.elapsed() > self.timeout {
                // Its guests notice that it is gone and exit
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut ending = Ending::parse(&reader.join().unwrap_or_default());
        ending.timed_out = timed_out;
        ending
    }
}

/// `f(slot, index)` for every index below `n`, `jobs` at a time; slot 0 is
/// left to the caller's own runs. Results in index order.
pub fn fan_out<T: Send>(jobs: u32, n: usize, f: impl Fn(u32, usize) -> T + Sync) -> Vec<T> {
    let jobs = jobs.clamp(1, 98) as usize;
    let mut out: Vec<Option<T>> = (0..n).map(|_| None).collect();
    std::thread::scope(|scope| {
        let f = &f;
        let workers: Vec<_> = (0..jobs.min(n))
            .map(|job| {
                scope.spawn(move || {
                    (job..n)
                        .step_by(jobs)
                        .map(|i| (i, f(job as u32 + 1, i)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for w in workers {
            for (i, value) in w.join().expect("a run") {
                out[i] = Some(value);
            }
        }
    });
    out.into_iter()
        .map(|v| v.expect("every index ran"))
        .collect()
}

/// The timeout for the runs that follow a reference run that took `took`.
#[must_use]
pub fn timeout_after(took: Duration) -> Duration {
    (took * 20).max(Duration::from_secs(10))
}

/// Virtual nanoseconds as milliseconds, for people
#[must_use]
pub fn ms(ns: u64) -> String {
    format!("{}.{:03} ms", ns / 1_000_000, ns / 1000 % 1000)
}

/// Lines of a schedule trace as (clock, line)
#[must_use]
pub fn trace_lines(path: &Path) -> Vec<(u64, String)> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            Some((
                l.rsplit_once("clock=")?.1.trim().parse().ok()?,
                l.to_string(),
            ))
        })
        .collect()
}
