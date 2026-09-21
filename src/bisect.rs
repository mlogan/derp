//! Seed bisection: when was a failing run's failure decided?
//!
//! The failing seed is replayed with every random stream reseeded at
//! virtual time `t`. Up to `t` the run is the failing run; after it, some
//! other future. Once the damage is done nearly every future fails; before,
//! a future fails only as often as any run does. The probability of failure
//! as a function of `t` therefore steps up where the damage is done, and a
//! binary search over `t`, with many futures per probe, finds the step.
//!
//! A probe is a noisy measurement. With a base rate near 13% and 20
//! futures a wrong turn is practically impossible; at 50% about one search
//! in ten takes one, and the closer the base rate is to 1 the less there
//! is to find. `--runs` buys certainty.

use std::time::Instant;

use crate::replay::{fan_out, ms, timeout_after, trace_lines, Ending, Replay};

pub struct Config {
    pub replay: Replay,
    pub seed: u64,
    /// Futures per probe
    pub runs: u32,
    /// Runs at a time
    pub jobs: u32,
    pub resolution_ns: u64,
}

#[derive(Debug)]
pub struct Probe {
    pub at_ns: u64,
    pub failed: u32,
    /// Futures that failed some other way, or hung
    pub otherwise: u32,
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

/// `cfg.runs` futures from time `at`: how many end as the reference did.
fn probe(cfg: &Config, reference: &Ending, at: u64) -> Probe {
    let endings = fan_out(cfg.jobs, cfg.runs as usize, |slot, k| {
        // Replacement seeds differ per probe as well: no future twice
        let with = at.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (k as u64 + 1);
        let extra = [
            "--reseed-at".into(),
            format!("{at}ns"),
            "--reseed".into(),
            with.to_string(),
        ];
        cfg.replay.run(slot, &extra, "")
    });
    let failed = endings.iter().filter(|e| e.fails_like(reference)).count() as u32;
    let otherwise = endings
        .iter()
        .filter(|e| !e.fails_like(reference) && (e.failure.is_some() || e.timed_out))
        .count() as u32;
    Probe {
        at_ns: at,
        failed,
        otherwise,
    }
}

/// # Errors
/// When the seed does not fail, or there is no step to find.
pub fn bisect(cfg: &mut Config, mut progress: impl FnMut(&str)) -> Result<Found, String> {
    if cfg.runs == 0 {
        return Err("--runs 0: a probe needs futures".into());
    }
    std::fs::create_dir_all(&cfg.replay.dir).map_err(|e| e.to_string())?;
    let began = Instant::now();
    let reference = cfg.replay.run(0, &[], "");
    cfg.replay.timeout = timeout_after(began.elapsed());
    let cfg = &*cfg;
    let Some((entry, status)) = reference.failure.clone() else {
        return Err(format!(
            "seed {} does not fail: nothing to bisect",
            cfg.seed
        ));
    };
    if reference.failure_at == 0 {
        return Err("the report does not say when the failing process died".into());
    }
    progress(&format!(
        "reference: seed {} fails (entry {entry}: {status}) at {}",
        cfg.seed,
        ms(reference.failure_at)
    ));

    let say = |p: &Probe, what: &str| {
        let other = if p.otherwise > 0 {
            format!(", {} end some other way", p.otherwise)
        } else {
            String::new()
        };
        format!(
            "probe {:>12}: {:>3} of {} fail{other}  {what}",
            ms(p.at_ns),
            p.failed,
            cfg.runs
        )
    };
    let base = probe(cfg, &reference, 1);
    progress(&format!(
        "base rate: {} of {} futures from the start fail the same way",
        base.failed, cfg.runs
    ));
    if base.failed * 5 >= cfg.runs * 4 {
        return Err(format!(
            "{} of {} futures fail even when reseeded at the start: this failure does not \
             depend on anything random enough to have a moment",
            base.failed, cfg.runs
        ));
    }
    // Halfway between what any run does and certainty
    let threshold = (base.failed + cfg.runs).div_ceil(2);

    // Is it decided at all before the process dies?
    let resolution = cfg.resolution_ns.max(1);
    let end = reference.failure_at.saturating_sub(resolution).max(1);
    let last = probe(cfg, &reference, end);
    let mut probes = Vec::new();
    let (mut lo, mut lo_failed, mut hi, mut hi_failed);
    if last.failed >= threshold {
        progress(&say(&last, "decided by then"));
        (lo, lo_failed, hi, hi_failed) = (1, base.failed, end, Some(last.failed));
    } else {
        progress(&say(&last, "still open"));
        (lo, lo_failed, hi, hi_failed) = (end, last.failed, reference.failure_at, None);
    }
    probes.push(last);
    while hi - lo > resolution {
        let p = probe(cfg, &reference, lo + (hi - lo) / 2);
        if p.failed >= threshold {
            progress(&say(&p, "decided by then"));
            (hi, hi_failed) = (p.at_ns, Some(p.failed));
        } else {
            progress(&say(&p, "still open"));
            (lo, lo_failed) = (p.at_ns, p.failed);
        }
        probes.push(p);
    }
    let after = hi_failed.map_or_else(
        || "it is only decided as the process dies".to_string(),
        |n| format!("{n} after"),
    );
    progress(&format!(
        "the failure is decided between {} and {} ({lo_failed} of {} fail before, {after})",
        ms(lo),
        ms(hi),
        cfg.runs
    ));

    let trace = trace_lines(&cfg.replay.trace_path(0))
        .into_iter()
        .filter(|(clock, _)| (lo..=hi).contains(clock))
        .map(|(_, line)| line)
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
