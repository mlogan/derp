//! Site minimisation: which loads and stores does a failing run need?
//!
//! Every stub stays in place and keeps counting, so hook counts, and with
//! them the schedule, do not shift. A *masked* site is one where a quantum
//! may not end; the switch falls on the next hook. Starting from the loads
//! and stores at which the failing run did switch, delta debugging finds a
//! set that is enough to reproduce the failure and from which no single
//! site can be dropped. Those are the suspects.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::rewrite::{Site, SiteKind};

/// A program of the run file
pub struct Program {
    /// File name, which is what a mask line and the supervisor go by
    pub name: String,
    /// The original binary, for symbols
    pub original: PathBuf,
    pub sites: Vec<Site>,
}

pub struct Config {
    pub manifest: PathBuf,
    pub scratch: PathBuf,
    pub seed: u64,
    pub jobs: u32,
    /// Options every run gets
    pub pass: Vec<String>,
    /// By run-file entry
    pub programs: Vec<Program>,
}

/// A load or store of a program: (index in `Config::programs`, yield pc)
type Key = (usize, u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suspect {
    pub program: String,
    pub addr: u64,
    pub kind: SiteKind,
    /// `atos` output: function, file and line when there are symbols
    pub location: String,
}

#[derive(Debug)]
pub struct Found {
    pub failure: String,
    pub candidates: usize,
    pub runs: u32,
    pub suspects: Vec<Suspect>,
}

struct Runner<'a> {
    cfg: &'a Config,
    failure: String,
    /// The sites being decided on: all of them are masked but the allowed
    universe: BTreeSet<Key>,
    runs: std::sync::atomic::AtomicU32,
}

struct RunResult {
    failure: Option<String>,
    stderr: String,
}

/// One run in which a switch may happen at the loads and stores in `allowed`
/// only (None: anywhere). The mask variable is always set, and its value is
/// as long for every job: the environment's size places the guest's stack.
fn run(cfg: &Config, job: u32, mask: &str, trace: Option<&Path>) -> RunResult {
    let dir = cfg.scratch.join("suspects");
    let mask_path = dir.join(format!("mask.{job:02}"));
    std::fs::write(&mask_path, mask).expect("writing the mask");
    let mut cmd = Command::new(std::env::current_exe().expect("own path"));
    cmd.args(["run", "--capture", "--seed", &cfg.seed.to_string()])
        .args(&cfg.pass)
        .arg("--scratch")
        .arg(dir.join(format!("job{job:02}")))
        .arg("--manifest")
        .arg(&cfg.manifest)
        .env("REWRITE_MASK", &mask_path);
    match trace {
        Some(path) => cmd.env("REWRITE_TRACE", path),
        None => cmd.env_remove("REWRITE_TRACE"),
    };
    let out = cmd.output().expect("running rewrite");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    RunResult {
        failure: crate::bisect::ending(&stderr).failure,
        stderr,
    }
}

impl Runner<'_> {
    fn mask_for(&self, allowed: &[Key]) -> String {
        use std::fmt::Write;
        let mut text = String::new();
        for &(p, pc) in self.universe.iter().filter(|k| !allowed.contains(k)) {
            let _ = writeln!(text, "{} {pc:x}", self.cfg.programs[p].name);
        }
        text
    }

    /// Whether each of `sets`, allowed alone, still fails the same way.
    fn test(&self, sets: &[Vec<Key>]) -> Vec<bool> {
        let jobs = self.cfg.jobs.max(1) as usize;
        let mut out = vec![false; sets.len()];
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..jobs)
                .map(|job| {
                    scope.spawn(move || {
                        (job..sets.len())
                            .step_by(jobs)
                            .map(|i| {
                                let r =
                                    run(self.cfg, job as u32 + 1, &self.mask_for(&sets[i]), None);
                                (i, r.failure.as_deref() == Some(&self.failure))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            for w in workers {
                for (i, fails) in w.join().expect("test run") {
                    out[i] = fails;
                }
            }
        });
        self.runs
            .fetch_add(sets.len() as u32, std::sync::atomic::Ordering::Relaxed);
        out
    }
}

/// Zeller's `ddmin`: a subset of `all` that still fails and from which no
/// single element can be removed. `progress` hears of every reduction.
fn ddmin(r: &Runner, all: Vec<Key>, progress: &mut impl FnMut(&str)) -> Vec<Key> {
    let mut current = all;
    let mut n = 2;
    while current.len() >= 2 {
        let chunk = current.len().div_ceil(n);
        let parts: Vec<Vec<Key>> = current.chunks(chunk).map(<[Key]>::to_vec).collect();
        let complements: Vec<Vec<Key>> = (0..parts.len())
            .map(|skip| {
                parts
                    .iter()
                    .enumerate()
                    .filter(|&(i, _)| i != skip)
                    .flat_map(|(_, p)| p.iter().copied())
                    .collect()
            })
            .collect();
        if let Some(i) = r.test(&parts).iter().position(|&fails| fails) {
            current.clone_from(&parts[i]);
            n = 2;
        } else if let Some(i) = (parts.len() > 2)
            .then(|| r.test(&complements).iter().position(|&fails| fails))
            .flatten()
        {
            current.clone_from(&complements[i]);
            n = (n - 1).max(2);
        } else if n >= current.len() {
            break;
        } else {
            n = (2 * n).min(current.len());
            continue;
        }
        progress(&format!("  still fails with {} sites", current.len()));
    }
    current
}

/// Function, file and line of `addrs` in `binary`, one string per address.
fn symbolize(binary: &Path, addrs: &[u64]) -> Vec<String> {
    let out = Command::new("atos")
        .arg("-o")
        .arg(binary)
        .args(["-arch", "arm64"])
        .args(addrs.iter().map(|a| format!("{a:#x}")))
        .output();
    let lines: Vec<String> = out
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    addrs
        .iter()
        .enumerate()
        .map(|(i, a)| lines.get(i).cloned().unwrap_or_else(|| format!("{a:#x}")))
        .collect()
}

/// The sites of `among` at which a traced run switched, in order of first
/// appearance. A process is its run-file entry's program.
fn switched_at(report: &str, trace: &Path, among: &BTreeSet<Key>) -> Vec<Key> {
    let entry_of: BTreeMap<String, usize> = report
        .lines()
        .filter_map(|l| l.split_once(".entry="))
        .filter_map(|(p, e)| Some((p.to_string(), e.parse().ok()?)))
        .collect();
    let mut out: Vec<Key> = Vec::new();
    for line in std::fs::read_to_string(trace).unwrap_or_default().lines() {
        let process = line.split(' ').next().unwrap_or_default();
        let site = line
            .split(' ')
            .find_map(|w| w.strip_prefix("site=0x"))
            .and_then(|v| u64::from_str_radix(v, 16).ok());
        if let (Some(&p), Some(pc)) = (entry_of.get(process), site) {
            if among.contains(&(p, pc)) && !out.contains(&(p, pc)) {
                out.push((p, pc));
            }
        }
    }
    out
}

/// # Errors
/// When the seed does not fail, or something cannot be run.
pub fn suspects(cfg: &Config, mut progress: impl FnMut(&str)) -> Result<Found, String> {
    let dir = cfg.scratch.join("suspects");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let trace_path = dir.join("reference.trace");
    let _ = std::fs::remove_file(&trace_path);
    let reference = run(cfg, 0, "", Some(&trace_path));
    let Some(failure) = reference.failure else {
        return Err(format!(
            "seed {} does not fail: nothing to minimise",
            cfg.seed
        ));
    };

    let of_kind = |memory: bool| -> BTreeSet<Key> {
        cfg.programs
            .iter()
            .enumerate()
            .flat_map(|(p, prog)| {
                prog.sites
                    .iter()
                    .filter(move |s| memory == matches!(s.kind, SiteKind::Load | SiteKind::Store))
                    .map(move |s| (p, s.yield_pc))
            })
            .collect()
    };
    let memory_sites = of_kind(true);
    let candidates = switched_at(&reference.stderr, &trace_path, &memory_sites);
    progress(&format!(
        "reference: seed {} fails ({failure}); it switched at {} of {} hooked loads and stores",
        cfg.seed,
        candidates.len(),
        memory_sites.len()
    ));

    let mut runner = Runner {
        cfg,
        failure: failure.clone(),
        universe: memory_sites.clone(),
        runs: 1.into(),
    };
    let checks = runner.test(&[candidates.clone(), Vec::new()]);
    if !checks[0] {
        return Err(
            "masking the loads and stores the run never switched at changed it: \
                    is something outside the schedule at work?"
                .into(),
        );
    }
    let (considered, minimal) = if checks[1] {
        // The same question of the branches and calls, in the run that has
        // no switch at a load or store
        progress("it fails without a switch at any load or store; trying branches and calls");
        let all: BTreeSet<Key> = memory_sites.union(&of_kind(false)).copied().collect();
        runner.universe = all;
        // The trace file is appended to
        let _ = std::fs::remove_file(&trace_path);
        let traced = run(
            cfg,
            0,
            &runner.mask_for(&of_kind(false).into_iter().collect::<Vec<_>>()),
            Some(&trace_path),
        );
        let candidates = switched_at(&traced.stderr, &trace_path, &of_kind(false));
        progress(&format!(
            "that run switched at {} hooked branches and calls",
            candidates.len()
        ));
        let checks = runner.test(&[candidates.clone(), Vec::new()]);
        if !checks[0] {
            return Err("masking unused branch and call sites changed the run".into());
        }
        if checks[1] {
            progress(
                "it fails with no switch at any hooked instruction: blocking calls are enough",
            );
            (candidates.len(), Vec::new())
        } else {
            (candidates.len(), ddmin(&runner, candidates, &mut progress))
        }
    } else {
        (candidates.len(), ddmin(&runner, candidates, &mut progress))
    };

    let mut suspects = Vec::new();
    for (p, prog) in cfg.programs.iter().enumerate() {
        let here: Vec<&Site> = prog
            .sites
            .iter()
            .filter(|s| minimal.contains(&(p, s.yield_pc)))
            .collect();
        let addrs: Vec<u64> = here.iter().map(|s| s.addr).collect();
        for (site, location) in here.iter().zip(symbolize(&prog.original, &addrs)) {
            suspects.push(Suspect {
                program: prog.name.clone(),
                addr: site.addr,
                kind: site.kind,
                location,
            });
        }
    }
    suspects.dedup();
    Ok(Found {
        failure,
        candidates: considered,
        runs: runner.runs.load(std::sync::atomic::Ordering::Relaxed),
        suspects,
    })
}
