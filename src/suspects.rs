//! Site minimisation: which loads and stores does a failing run need?
//!
//! Every stub stays in place and keeps counting, so hook counts, and with
//! them the schedule, do not shift. A *masked* site is one where a quantum
//! may not end; the switch falls on the next hook. Starting from the loads
//! and stores at which the failing run did switch, delta debugging finds a
//! set that is enough to reproduce the failure and from which no single
//! site can be dropped. Those are the suspects.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::replay::{fan_out, timeout_after, trace_lines, Ending, Replay};
use crate::rewrite::{Site, SiteKind};

/// A distinct program of the run file
pub struct Program {
    /// File name, which is what a mask line and the supervisor go by
    pub name: String,
    /// The original binary, for symbols
    pub original: PathBuf,
    pub sites: Vec<Site>,
}

pub struct Config {
    pub replay: Replay,
    pub seed: u64,
    pub jobs: u32,
    pub programs: Vec<Program>,
    /// Which of `programs` each run-file entry runs
    pub program_of_entry: Vec<usize>,
}

/// A hook site of a program: (index in `Config::programs`, yield pc)
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
    pub candidates: usize,
    pub runs: u32,
    pub suspects: Vec<Suspect>,
}

struct Runner<'a> {
    cfg: &'a Config,
    reference: Ending,
    /// The sites being decided on: all of them are masked but the allowed
    universe: BTreeSet<Key>,
    runs: std::sync::atomic::AtomicU32,
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

    /// How the run ends when a switch may happen at each of `sets` alone.
    fn endings(&self, sets: &[Vec<Key>]) -> Vec<Ending> {
        self.runs
            .fetch_add(sets.len() as u32, std::sync::atomic::Ordering::Relaxed);
        fan_out(self.cfg.jobs, sets.len(), |slot, i| {
            self.cfg.replay.run(slot, &[], &self.mask_for(&sets[i]))
        })
    }

    fn test(&self, sets: &[Vec<Key>]) -> Vec<bool> {
        self.endings(sets)
            .iter()
            .map(|e| e.fails_like(&self.reference))
            .collect()
    }

    /// The two runs that must come out as expected before anything is
    /// concluded: allowed exactly where it switched, the run is the traced
    /// run, hash and all; allowed nowhere, it is some other run, or the mask
    /// reached nobody. Returns whether it still fails with none allowed.
    fn check(&self, traced: &Ending, candidates: &[Key]) -> Result<bool, String> {
        let e = self.endings(&[candidates.to_vec(), Vec::new()]);
        if e[0].schedule_hash != traced.schedule_hash {
            return Err("masking sites the run never switched at changed the run: \
                        is something outside the schedule at work?"
                .into());
        }
        if !candidates.is_empty() && e[1].schedule_hash == traced.schedule_hash {
            return Err(
                "masking every site a quantum ended at changed nothing: the mask did \
                        not reach the guests, or the run spins in a loop of masked sites \
                        (the supervisor then says so)"
                    .into(),
            );
        }
        Ok(e[1].fails_like(&self.reference))
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
fn switched_at(cfg: &Config, ending: &Ending, trace: &Path, among: &BTreeSet<Key>) -> Vec<Key> {
    let mut out: Vec<Key> = Vec::new();
    for (_, line) in trace_lines(trace) {
        let process = line.split(' ').next().unwrap_or_default();
        let site = line
            .split(' ')
            .find_map(|w| w.strip_prefix("site=0x"))
            .and_then(|v| u64::from_str_radix(v, 16).ok());
        let program = ending
            .entry_of
            .get(process)
            .and_then(|&entry| cfg.program_of_entry.get(entry));
        if let (Some(&p), Some(pc)) = (program, site) {
            if among.contains(&(p, pc)) && !out.contains(&(p, pc)) {
                out.push((p, pc));
            }
        }
    }
    out
}

/// # Errors
/// When the seed does not fail, or a sanity check of the masking does.
pub fn suspects(cfg: &mut Config, mut progress: impl FnMut(&str)) -> Result<Found, String> {
    std::fs::create_dir_all(&cfg.replay.dir).map_err(|e| e.to_string())?;
    let began = Instant::now();
    let reference = cfg.replay.run(0, &[], "");
    cfg.replay.timeout = timeout_after(began.elapsed());
    let cfg = &*cfg;
    let Some((entry, status)) = reference.failure.clone() else {
        return Err(format!(
            "seed {} does not fail: nothing to minimise",
            cfg.seed
        ));
    };
    let trace = cfg.replay.trace_path(0);

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
    let (memory_sites, other_sites) = (of_kind(true), of_kind(false));
    let candidates = switched_at(cfg, &reference, &trace, &memory_sites);
    progress(&format!(
        "reference: seed {} fails (entry {entry}: {status}); a quantum ended at {} of {} hooked loads and stores",
        cfg.seed,
        candidates.len(),
        memory_sites.len()
    ));

    let mut runner = Runner {
        cfg,
        reference: reference.clone(),
        universe: memory_sites.clone(),
        runs: 1.into(),
    };
    let (considered, minimal) = if runner.check(&reference, &candidates)? {
        // The same question of the branches and calls, in the run that has
        // no switch at a load or store
        progress("it fails without a switch at any load or store; trying branches and calls");
        runner.universe = memory_sites.union(&other_sites).copied().collect();
        let all_others: Vec<Key> = other_sites.iter().copied().collect();
        let traced = cfg.replay.run(0, &[], &runner.mask_for(&all_others));
        let candidates = switched_at(cfg, &traced, &trace, &other_sites);
        progress(&format!(
            "in that run a quantum ended at {} hooked branches and calls",
            candidates.len()
        ));
        if runner.check(&traced, &candidates)? {
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
    Ok(Found {
        candidates: considered,
        runs: runner.runs.load(std::sync::atomic::Ordering::Relaxed),
        suspects,
    })
}
