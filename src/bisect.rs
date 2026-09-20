//! Seed bisection: when was a failing run's failure decided?
//!
//! The failing seed is replayed with the scheduler's streams reseeded at
//! virtual time `t`. Up to `t` the run is the failing run; after it, some
//! other future. Once the damage is done nearly every future fails; before,
//! a future fails only as often as any run does. The probability of failure
//! as a function of `t` therefore steps up where the damage is done, and a
//! binary search over `t`, with many futures per probe, finds the step.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Config {
    pub manifest: PathBuf,
    pub scratch: PathBuf,
    pub seed: u64,
    /// Futures per probe
    pub runs: u32,
    /// Runs at a time
    pub jobs: u32,
    pub resolution_ns: u64,
    /// Options every run gets (`--quantum`, `--mem-hook-rate`, …)
    pub pass: Vec<String>,
}

/// What a run's report says about how it ended
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ending {
    /// `entry <n>: <status>` of the run-file entry that failed, if one did
    pub failure: Option<String>,
    pub failure_at: u64,
}

#[derive(Debug)]
pub struct Probe {
    pub at_ns: u64,
    pub failed: u32,
    pub runs: u32,
}

#[derive(Debug)]
pub struct Found {
    pub reference: Ending,
    pub base: Probe,
    pub probes: Vec<Probe>,
    /// Still open at `lo`, decided by `hi`
    pub lo_ns: u64,
    pub hi_ns: u64,
    pub trace: Vec<String>,
}

fn ending(stderr: &str) -> Ending {
    let field = |key: &str| {
        stderr
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(str::to_string)
    };
    Ending {
        failure: field("run.failure"),
        failure_at: field("run.failure_at")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    }
}

fn one_run(
    cfg: &Config,
    scratch: &Path,
    reseed: Option<(u64, u64)>,
    trace: Option<&Path>,
) -> Ending {
    let mut cmd = Command::new(std::env::current_exe().expect("own path"));
    cmd.args(["run", "--capture", "--seed", &cfg.seed.to_string()])
        .args(&cfg.pass)
        .arg("--scratch")
        .arg(scratch)
        .arg("--manifest")
        .arg(&cfg.manifest);
    if let Some((at, with)) = reseed {
        cmd.args([
            "--reseed-at",
            &format!("{at}ns"),
            "--reseed",
            &with.to_string(),
        ]);
    }
    match trace {
        Some(path) => cmd.env("REWRITE_TRACE", path),
        None => cmd.env_remove("REWRITE_TRACE"),
    };
    let out = cmd.output().expect("running rewrite");
    ending(&String::from_utf8_lossy(&out.stderr))
}

/// `cfg.runs` futures from time `at`: how many end as the reference did.
fn probe(cfg: &Config, reference: &Ending, at: u64) -> Probe {
    let jobs = cfg.jobs.max(1);
    let failed = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..jobs)
            .map(|job| {
                scope.spawn(move || {
                    let scratch = cfg.scratch.join(format!("bisect/job{job}"));
                    (0..cfg.runs)
                        .filter(|k| k % jobs == job)
                        // Replacement seeds differ per probe as well, so that
                        // no future is sampled twice
                        .filter(|&k| {
                            let with = at.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ u64::from(k + 1);
                            one_run(cfg, &scratch, Some((at, with)), None).failure
                                == reference.failure
                        })
                        .count() as u32
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().expect("probe")).sum()
    });
    Probe {
        at_ns: at,
        failed,
        runs: cfg.runs,
    }
}

/// # Errors
/// When the seed does not fail, or fails so often from the start that there
/// is no step to find.
pub fn bisect(cfg: &Config, mut progress: impl FnMut(&str)) -> Result<Found, String> {
    let trace_path = cfg.scratch.join("bisect/reference.trace");
    std::fs::create_dir_all(cfg.scratch.join("bisect")).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&trace_path);
    let reference = one_run(
        cfg,
        &cfg.scratch.join("bisect/reference"),
        None,
        Some(&trace_path),
    );
    let Some(failure) = reference.failure.clone() else {
        return Err(format!(
            "seed {} does not fail: nothing to bisect",
            cfg.seed
        ));
    };
    progress(&format!(
        "reference: seed {} fails ({failure}) at {}",
        cfg.seed,
        ms(reference.failure_at)
    ));

    let base = probe(cfg, &reference, 1);
    progress(&format!(
        "base rate: {} of {} futures from the start fail the same way",
        base.failed, base.runs
    ));
    // Halfway between what any run does and certainty
    let threshold = (base.failed + base.runs).div_ceil(2);
    if base.failed * 5 >= base.runs * 4 {
        return Err(format!(
            "{} of {} futures fail even when reseeded at the start: this failure does not \
             depend on the schedule enough to have a moment",
            base.failed, base.runs
        ));
    }

    let (mut lo, mut hi) = (0u64, reference.failure_at);
    let (mut lo_failed, mut hi_failed) = (base.failed, base.runs);
    let mut probes = Vec::new();
    while hi - lo > cfg.resolution_ns.max(1) {
        let p = probe(cfg, &reference, lo + (hi - lo) / 2);
        let decided = p.failed >= threshold;
        progress(&format!(
            "probe {:>12}: {:>3} of {} fail  {}",
            ms(p.at_ns),
            p.failed,
            p.runs,
            if decided {
                "decided by then"
            } else {
                "still open"
            }
        ));
        if decided {
            (hi, hi_failed) = (p.at_ns, p.failed);
        } else {
            (lo, lo_failed) = (p.at_ns, p.failed);
        }
        probes.push(p);
    }
    progress(&format!(
        "the failure is decided between {} and {} ({lo_failed} of {} fail before, {hi_failed} after)",
        ms(lo),
        ms(hi),
        cfg.runs
    ));

    let clock = |line: &str| line.rsplit_once("clock=")?.1.trim().parse::<u64>().ok();
    let trace = std::fs::read_to_string(&trace_path)
        .unwrap_or_default()
        .lines()
        .filter(|l| clock(l).is_some_and(|c| c >= lo && c <= hi))
        .map(str::to_string)
        .collect();
    Ok(Found {
        reference,
        base,
        probes,
        lo_ns: lo,
        hi_ns: hi,
        trace,
    })
}

/// Virtual nanoseconds as milliseconds, for people
#[must_use]
pub fn ms(ns: u64) -> String {
    format!("{}.{:03} ms", ns / 1_000_000, ns / 1000 % 1000)
}
