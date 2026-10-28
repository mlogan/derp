//! The run file: which processes start on which virtual host, and the
//! settings of the run. YAML:
//!
//! ```yaml
//! seed: 7                  # optional; the command line overrides these
//! quantum: 1000..10000
//! heap-size: 32G           # address space of each guest's heap
//! heap-release: on         # freed runs' pages go back to the system
//! mem-hook-rate: 1/16
//! net-latency: 5ms
//! switch-cost: 10us        # virtual time a baton hand-off costs
//! allow:                   # extra paths every host may touch
//!   - /opt/site-content
//! env: { LOG_LEVEL: debug } # for every process; guests start from a fixed
//! pass-env: [SSL_CERT_FILE] # environment, and inherit only what is named
//! hosts:                   # in order: 10.0.0.1, 10.0.0.2, ...
//!   - name: alpha
//!     files: [site/index.html, site/img]   # copied into the host's directory
//!     processes:
//!       - [server, --port, 8080]        # argv verbatim
//!       - client alpha 8080             # or a line, split on whitespace
//!       - argv: [worker, "two words"]   # or a map, with an environment
//!         env: { MODE: fast }
//!       - argv: [httpd, --port, 80]     # a server that never exits: killed
//!         daemon: true                  # when all other processes are done
//!         restart: on-failure           # never (default) | on-failure | always
//!         restart-delay: 200ms..1s      # virtual time it stays down (default 100ms)
//!         max-restarts: 10              # default: no limit
//!         crash:
//!           every: 50ms..200ms          # into each life, drawn from the seed
//!           times: 3                    # default: no limit
//! ```
//!
//! Processes start in file order. `argv[0]` names the program, relative to
//! the run file, and reaches the guest as written. A host's directory is
//! never named here: the launcher makes a fresh one for every run.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::shared::{self, Faults};

/// A YAML scalar used as an argument. Numbers and booleans are taken as
/// their text; quote one whose spelling matters (`"1.10"`, `"007"`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Scalar {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

impl Scalar {
    fn text(&self) -> String {
        match self {
            Scalar::Bool(b) => b.to_string(),
            Scalar::Int(i) => i.to_string(),
            Scalar::Float(f) => f.to_string(),
            Scalar::Text(s) => s.clone(),
        }
    }
}

#[derive(Debug)]
enum RawProcess {
    /// Split on whitespace; use the list form for arguments with spaces
    Line(String),
    Argv(Vec<Scalar>),
    Full(RawFull),
}

/// By the YAML node's shape rather than `untagged`, which would answer a
/// mistyped key in the map form with "did not match any variant".
impl<'de> Deserialize<'de> for RawProcess {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        use serde_yaml::Value;
        let v = Value::deserialize(d)?;
        match v {
            Value::String(line) => Ok(RawProcess::Line(line)),
            Value::Sequence(_) => serde_yaml::from_value(v).map(RawProcess::Argv),
            Value::Mapping(_) => serde_yaml::from_value(v).map(RawProcess::Full),
            _ => {
                return Err(D::Error::custom(
                    "a process is a line, a list (argv) or a map with argv:",
                ))
            }
        }
        .map_err(D::Error::custom)
    }
}

/// Its own struct so that a mistyped key is an error: `deamon: true` must
/// not quietly mean "not a daemon".
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFull {
    argv: Vec<Scalar>,
    #[serde(default)]
    env: BTreeMap<String, Scalar>,
    #[serde(default)]
    daemon: bool,
    restart: Option<String>,
    #[serde(rename = "restart-delay")]
    restart_delay: Option<Scalar>,
    #[serde(rename = "max-restarts")]
    max_restarts: Option<u32>,
    crash: Option<RawCrash>,
    shutdown: Option<RawShutdown>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCrash {
    every: Scalar,
    times: Option<u32>,
}

/// `pause: off`, or how often and how long
#[derive(Debug)]
enum RawPause {
    Off(String),
    Full(RawPauseFull),
}

/// By shape, like `RawProcess`
impl<'de> Deserialize<'de> for RawPause {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        use serde_yaml::Value;
        let v = Value::deserialize(d)?;
        match v {
            Value::Mapping(_) => serde_yaml::from_value(v)
                .map(RawPause::Full)
                .map_err(D::Error::custom),
            _ => Scalar::deserialize(v)
                .map(|s| RawPause::Off(s.text()))
                .map_err(D::Error::custom),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPauseFull {
    chance: Scalar,
    #[serde(rename = "for")]
    length: Option<Scalar>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPct {
    depth: Option<u32>,
    horizon: Option<Scalar>,
    starve: Option<Scalar>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawShutdown {
    every: Scalar,
    times: Option<u32>,
    signal: Option<String>,
    grace: Option<Scalar>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHost {
    name: String,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    processes: Vec<RawProcess>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawRun {
    seed: Option<u64>,
    quantum: Option<String>,
    mem_hook_rate: Option<Scalar>,
    net_latency: Option<Scalar>,
    switch_cost: Option<Scalar>,
    heap_size: Option<Scalar>,
    heap_release: Option<Scalar>,
    stop_after: Option<Scalar>,
    wall_limit: Option<Scalar>,
    pause: Option<RawPause>,
    scheduler: Option<String>,
    pct: Option<RawPct>,
    outside_network: Option<String>,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, Scalar>,
    #[serde(default)]
    pass_env: Vec<String>,
    hosts: Vec<RawHost>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    /// Index into `Manifest::hosts`
    pub host: u32,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    /// A server that never exits by itself: the run ends, and it is
    /// killed, when every process that is not a daemon has exited
    pub daemon: bool,
    /// Crashes to inject and what happens after a death
    pub faults: Faults,
}

/// How long a process stays down when `restart-delay` is not given. Not
/// zero: a restart is never instantaneous.
const DEFAULT_RESTART_DELAY_NS: u64 = 100_000_000;

/// How long a shut-down process has to exit before it is crashed
const DEFAULT_GRACE_NS: u64 = 10_000_000_000;

/// The signals a shutdown may send, by name with or without `SIG`
fn shutdown_signal(name: &str) -> Option<u8> {
    let sig = match name.strip_prefix("SIG").unwrap_or(name) {
        "TERM" => libc::SIGTERM,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "HUP" => libc::SIGHUP,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        _ => return None,
    };
    Some(sig as u8)
}

/// A year
const MAX_DURATION_NS: u64 = 365 * 24 * 3600 * 1_000_000_000;

/// A heap size: `32G`, `512M`, `4T`, or bytes. Between 64 MB and 4 TB.
#[must_use]
pub fn parse_size(s: &str) -> Option<u64> {
    let digits = s.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let shift = match &s[digits.len()..] {
        "" => 0,
        "K" | "k" => 10,
        "M" | "m" => 20,
        "G" | "g" => 30,
        "T" | "t" => 40,
        _ => return None,
    };
    let bytes = digits.parse::<u64>().ok()?.checked_mul(1 << shift)?;
    ((64 << 20)..=(4 << 40)).contains(&bytes).then_some(bytes)
}

/// How a duration is written, for error messages
pub const DURATION_FORMAT: &str =
    "a whole number with ns, us, ms or s, such as 250us; a bare number is milliseconds";

/// `5ms`, `250us`, `10ns`, `1s`; a bare number is milliseconds.
pub fn parse_duration_ns(s: &str) -> Option<u64> {
    let digits = s.trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == 'µ');
    let scale = match &s[digits.len()..] {
        "ns" => 1,
        "us" | "µs" => 1_000,
        "" | "ms" => 1_000_000,
        "s" => 1_000_000_000,
        _ => return None,
    };
    let ns = digits.parse::<u64>().ok()?.checked_mul(scale)?;
    // Far from overflowing the virtual clock when added to it
    (ns <= MAX_DURATION_NS).then_some(ns)
}

/// `random` or `pct`
#[must_use]
pub fn parse_scheduler(s: &str) -> Option<u8> {
    match s {
        "random" => Some(shared::SCHED_RANDOM),
        "pct" => Some(shared::SCHED_PCT),
        _ => None,
    }
}

#[must_use]
pub fn parse_pct_depth(d: u32) -> Option<u32> {
    (1..=shared::MAX_PCT_DEPTH).contains(&d).then_some(d)
}

/// A duration that is not zero
#[must_use]
pub fn parse_positive(s: &str) -> Option<u64> {
    parse_duration_ns(s).filter(|&ns| ns > 0)
}

/// How often a scheduling choice pauses a thread: `off`, or `N/M`
#[must_use]
pub fn parse_chance(s: &str) -> Option<(u32, u32)> {
    if s == "off" {
        return Some((0, 1));
    }
    let (num, den) = s.split_once('/')?;
    let (num, den) = (
        num.trim().parse::<u32>().ok()?,
        den.trim().parse::<u32>().ok()?,
    );
    (den > 0 && num <= den).then_some((num, den))
}

/// How long a pause lasts: a range whose low end is not zero, since the
/// lengths are drawn log-uniformly
#[must_use]
pub fn parse_pause_for(s: &str) -> Option<(u64, u64)> {
    parse_duration_range(s).filter(|&(lo, _)| lo > 0)
}

/// `50ms..200ms`, or one duration for both ends
fn parse_duration_range(s: &str) -> Option<(u64, u64)> {
    let (lo, hi) = s.split_once("..").unwrap_or((s, s));
    let (lo, hi) = (parse_duration_ns(lo.trim())?, parse_duration_ns(hi.trim())?);
    (lo <= hi).then_some((lo, hi))
}

fn faults_of(full: &RawFull) -> Result<Faults, String> {
    let restart = match full.restart.as_deref() {
        None | Some("never") => shared::RESTART_NEVER,
        Some("on-failure") => shared::RESTART_ON_FAILURE,
        Some("always") => shared::RESTART_ALWAYS,
        Some(other) => {
            return Err(format!(
                "run file: restart: {other} (expected never, on-failure or always)"
            ))
        }
    };
    if restart == shared::RESTART_NEVER
        && (full.restart_delay.is_some() || full.max_restarts.is_some())
    {
        return Err("run file: restart-delay and max-restarts need a restart policy".into());
    }
    let (delay_lo, delay_hi) = match &full.restart_delay {
        None => (DEFAULT_RESTART_DELAY_NS, DEFAULT_RESTART_DELAY_NS),
        Some(d) => parse_duration_range(&d.text())
            .filter(|&(lo, _)| lo > 0)
            .ok_or(format!("run file: bad restart-delay {}", d.text()))?,
    };
    let (crash_lo, crash_hi, crashes) = match &full.crash {
        None => (0, 0, 0),
        Some(c) => {
            let (lo, hi) = parse_duration_range(&c.every.text())
                .filter(|&(lo, _)| lo > 0)
                .ok_or(format!("run file: bad crash every {}", c.every.text()))?;
            (lo, hi, c.times.unwrap_or(shared::NO_LIMIT))
        }
    };
    let mut shutdown = Faults::default();
    if let Some(sd) = &full.shutdown {
        let (lo, hi) = parse_duration_range(&sd.every.text())
            .filter(|&(lo, _)| lo > 0)
            .ok_or(format!("run file: bad shutdown every {}", sd.every.text()))?;
        let signal = sd.signal.as_deref().unwrap_or("TERM");
        shutdown.shutdown_signal = shutdown_signal(signal).ok_or(format!(
            "run file: shutdown signal {signal} (TERM, INT, QUIT, HUP, USR1 or USR2)"
        ))?;
        shutdown.grace_ns = match &sd.grace {
            None => DEFAULT_GRACE_NS,
            Some(g) => parse_duration_ns(&g.text())
                .filter(|&ns| ns > 0)
                .ok_or(format!("run file: bad shutdown grace {}", g.text()))?,
        };
        (shutdown.shutdown_lo_ns, shutdown.shutdown_hi_ns) = (lo, hi);
        shutdown.shutdowns_left = sd.times.unwrap_or(shared::NO_LIMIT);
    }
    Ok(Faults {
        restart,
        restart_delay_lo_ns: delay_lo,
        restart_delay_hi_ns: delay_hi,
        restarts_left: if restart == shared::RESTART_NEVER {
            0
        } else {
            full.max_restarts.unwrap_or(shared::NO_LIMIT)
        },
        crash_lo_ns: crash_lo,
        crash_hi_ns: crash_hi,
        crashes_left: crashes,
        ..shutdown
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub name: String,
    /// Inputs copied into the host's fresh directory, relative to the run file
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub hosts: Vec<Host>,
    pub processes: Vec<Process>,
    pub seed: Option<u64>,
    /// As on the command line: `LO..HI`, `1/16`, `5ms`
    pub quantum: Option<String>,
    pub mem_hook_rate: Option<String>,
    pub net_latency: Option<String>,
    /// Virtual time a baton hand-off costs
    pub switch_cost: Option<String>,
    pub heap_size: Option<String>,
    /// `on` or `off`
    pub heap_release: Option<String>,
    /// Virtual time at which the run is over, whatever still runs
    pub stop_after: Option<String>,
    /// Real time after which the run is over, whatever still runs
    pub wall_limit: Option<String>,
    /// As on the command line: `off` or `1/200`, and `10us..100ms`
    pub pause_chance: Option<String>,
    pub pause_for: Option<String>,
    /// `random` or `pct`, and PCT's settings as on the command line
    pub scheduler: Option<String>,
    pub pct_depth: Option<u32>,
    pub pct_horizon: Option<String>,
    pub pct_starve: Option<String>,
    /// Whether a guest may connect to addresses outside the virtual
    /// network (what comes back is input the run cannot repeat); refused
    /// with `ENETUNREACH` unless the run file says `outside-network: allow`
    pub outside_network: bool,
    pub allow: Vec<String>,
    /// Variables for every process of the run
    pub env: Vec<(String, String)>,
    /// Variables every process inherits from the launcher's environment.
    /// Guests start from a fixed environment; this is how an input from
    /// outside is let in on purpose, and on the record.
    pub pass_env: Vec<String>,
}

/// The supervisor's own variables: a guest told otherwise would attach as
/// another process, or not at all.
fn check_names<'a>(names: impl Iterator<Item = &'a String>) -> Result<(), String> {
    for name in names {
        if name.starts_with("REWRITE_") || name.starts_with("DYLD_") {
            return Err(format!("run file: {name} is reserved for the supervisor"));
        }
    }
    Ok(())
}

/// As many as the shared state has room for
const MAX_HOSTS: usize = 32;
const MAX_PROCESSES: usize = 256;

/// A host's name is also the name of its directory in the scratch
/// directory, next to the launcher's own `stdout.<n>` files.
fn valid_host_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && !name.starts_with('.')
        && !name.starts_with("stdout.")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

pub fn parse(text: &str) -> Result<Manifest, String> {
    let raw: RawRun = serde_yaml::from_str(text).map_err(|e| format!("run file: {e}"))?;
    check_names(raw.env.keys())?;
    check_names(raw.pass_env.iter())?;
    if raw.hosts.len() > MAX_HOSTS {
        return Err(format!("run file: more than {MAX_HOSTS} hosts"));
    }
    let outside_network = match raw.outside_network.as_deref() {
        None | Some("refuse") => false,
        Some("allow") => true,
        Some(other) => {
            return Err(format!(
                "run file: outside-network is refuse (default) or allow, not {other}"
            ))
        }
    };
    let mut m = Manifest {
        seed: raw.seed,
        quantum: raw.quantum,
        mem_hook_rate: raw.mem_hook_rate.as_ref().map(Scalar::text),
        net_latency: raw.net_latency.as_ref().map(Scalar::text),
        switch_cost: raw.switch_cost.as_ref().map(Scalar::text),
        heap_size: raw.heap_size.as_ref().map(Scalar::text),
        heap_release: raw.heap_release.as_ref().map(Scalar::text),
        stop_after: raw.stop_after.as_ref().map(Scalar::text),
        wall_limit: raw.wall_limit.as_ref().map(Scalar::text),
        pause_chance: match &raw.pause {
            None => None,
            Some(RawPause::Off(s)) => Some(s.clone()),
            Some(RawPause::Full(p)) => Some(p.chance.text()),
        },
        pause_for: match &raw.pause {
            Some(RawPause::Full(p)) => p.length.as_ref().map(Scalar::text),
            _ => None,
        },
        scheduler: raw.scheduler.clone(),
        pct_depth: raw.pct.as_ref().and_then(|p| p.depth),
        pct_horizon: raw
            .pct
            .as_ref()
            .and_then(|p| p.horizon.as_ref().map(Scalar::text)),
        pct_starve: raw
            .pct
            .as_ref()
            .and_then(|p| p.starve.as_ref().map(Scalar::text)),
        outside_network,
        allow: raw.allow,
        env: raw.env.iter().map(|(k, v)| (k.clone(), v.text())).collect(),
        pass_env: raw.pass_env,
        ..Manifest::default()
    };
    if let Some(c) = m
        .pause_chance
        .as_deref()
        .filter(|c| parse_chance(c).is_none())
    {
        return Err(format!("run file: pause: bad chance {c} (off, or N/M)"));
    }
    if let Some(f) = m
        .pause_for
        .as_deref()
        .filter(|f| parse_pause_for(f).is_none())
    {
        return Err(format!("run file: pause: bad for {f} (a range above zero)"));
    }
    if let Some(k) = m
        .scheduler
        .as_deref()
        .filter(|k| parse_scheduler(k).is_none())
    {
        return Err(format!("run file: scheduler: {k} (expected random or pct)"));
    }
    if let Some(d) = m.pct_depth.filter(|&d| parse_pct_depth(d).is_none()) {
        return Err(format!(
            "run file: pct: depth {d} (1 to {})",
            shared::MAX_PCT_DEPTH
        ));
    }
    for (what, v) in [("horizon", &m.pct_horizon), ("starve", &m.pct_starve)] {
        if let Some(v) = v.as_deref().filter(|v| parse_positive(v).is_none()) {
            return Err(format!("run file: pct: bad {what} {v}"));
        }
    }
    for host in raw.hosts {
        if !valid_host_name(&host.name) {
            return Err(format!(
                "run file: host name {:?} must be letters, digits, '-' or '.'",
                host.name
            ));
        }
        // Directories: the file system may not tell `A` from `a`
        if m.hosts
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case(&host.name))
        {
            return Err(format!("run file: host {} declared twice", host.name));
        }
        let index = m.hosts.len() as u32;
        for p in host.processes {
            let (argv, env, daemon, faults): (Vec<String>, Vec<(String, String)>, bool, Faults) =
                match p {
                    RawProcess::Line(line) => (
                        line.split_whitespace().map(str::to_string).collect(),
                        Vec::new(),
                        false,
                        Faults::default(),
                    ),
                    RawProcess::Argv(argv) => (
                        argv.iter().map(Scalar::text).collect(),
                        Vec::new(),
                        false,
                        Faults::default(),
                    ),
                    RawProcess::Full(full) => {
                        check_names(full.env.keys())?;
                        (
                            full.argv.iter().map(Scalar::text).collect(),
                            full.env
                                .iter()
                                .map(|(k, v)| (k.clone(), v.text()))
                                .collect(),
                            full.daemon,
                            faults_of(&full)?,
                        )
                    }
                };
            if argv.is_empty() {
                return Err(format!(
                    "run file: host {} has a process with no program",
                    host.name
                ));
            }
            m.processes.push(Process {
                host: index,
                argv,
                env,
                daemon,
                faults,
            });
        }
        m.hosts.push(Host {
            name: host.name,
            files: host.files,
        });
    }
    if m.processes.is_empty() {
        return Err("run file: no processes".into());
    }
    if m.processes.len() > MAX_PROCESSES {
        return Err(format!("run file: more than {MAX_PROCESSES} processes"));
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("32G"), Some(32 << 30));
        assert_eq!(parse_size("512m"), Some(512 << 20));
        assert_eq!(parse_size("4T"), Some(4 << 40));
        assert_eq!(parse_size("134217728"), Some(128 << 20));
        for bad in ["", "G", "1M", "5T", "32GB", "-1G", "1.5G", "99999999999G"] {
            assert_eq!(parse_size(bad), None, "{bad}");
        }
    }

    #[test]
    fn hosts_processes_and_settings() {
        let m = parse(
            r#"
seed: 7
quantum: 10000..100000
mem-hook-rate: 1/16
net-latency: 5ms
allow: [/opt/content]
hosts:
  - name: alpha
    files: [site/index.html, site/img]
    processes:
      - [./srv, --port, 80, "two words", "007", true]
  - name: beta
    processes:
      - ./cli alpha   80
      - argv: [./cli, --, --seed, 3]
        env: { MODE: fast, LEVEL: 2 }
"#,
        )
        .unwrap();
        assert_eq!(m.hosts.len(), 2);
        assert_eq!(m.hosts[0].name, "alpha");
        assert_eq!(m.hosts[0].files, ["site/index.html", "site/img"]);
        assert!(m.hosts[1].files.is_empty());
        assert_eq!(m.processes[0].host, 0);
        assert_eq!(
            m.processes[0].argv,
            ["./srv", "--port", "80", "two words", "007", "true"]
        );
        assert_eq!(m.processes[1].host, 1);
        assert_eq!(m.processes[1].argv, ["./cli", "alpha", "80"]);
        assert_eq!(m.processes[2].argv, ["./cli", "--", "--seed", "3"]);
        assert!(!m.processes[2].daemon);
        let d = parse("hosts: [{name: a, processes: [{argv: [srv], daemon: true}, cli]}]").unwrap();
        assert!(d.processes[0].daemon && !d.processes[1].daemon);
        assert_eq!(
            m.processes[2].env,
            [
                ("LEVEL".to_string(), "2".to_string()),
                ("MODE".to_string(), "fast".to_string())
            ]
        );
        assert_eq!(m.seed, Some(7));
        assert_eq!(m.quantum.as_deref(), Some("10000..100000"));
        assert_eq!(m.mem_hook_rate.as_deref(), Some("1/16"));
        assert_eq!(m.net_latency.as_deref(), Some("5ms"));
        assert_eq!(m.allow, ["/opt/content"]);
        assert!(m.env.is_empty() && m.pass_env.is_empty());
        let e = parse(
            "env: {A: 1, B: two}\npass-env: [SSL_CERT_FILE]\nhosts: [{name: a, processes: [p]}]",
        )
        .unwrap();
        assert_eq!(
            e.env,
            [
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "two".to_string())
            ]
        );
        assert_eq!(e.pass_env, ["SSL_CERT_FILE"]);
    }

    #[test]
    fn crashes_and_restarts() {
        let m = parse(
            r"hosts:
  - name: a
    processes:
      - argv: [srv]
        restart: on-failure
        restart-delay: 200ms..1s
        max-restarts: 4
        crash: { every: 50ms..200ms, times: 3 }
      - argv: [cli]
        restart: always
        crash: { every: 1s }
      - plain
",
        )
        .unwrap();
        assert_eq!(
            m.processes[0].faults,
            Faults {
                restart: shared::RESTART_ON_FAILURE,
                restart_delay_lo_ns: 200_000_000,
                restart_delay_hi_ns: 1_000_000_000,
                restarts_left: 4,
                crash_lo_ns: 50_000_000,
                crash_hi_ns: 200_000_000,
                crashes_left: 3,
                ..Faults::default()
            }
        );
        let f = m.processes[1].faults;
        assert_eq!(f.restart, shared::RESTART_ALWAYS);
        assert_eq!(
            f.restart_delay_lo_ns, 100_000_000,
            "never instantaneous by default"
        );
        assert_eq!(
            (f.restarts_left, f.crashes_left),
            (shared::NO_LIMIT, shared::NO_LIMIT)
        );
        assert_eq!(
            (f.crash_lo_ns, f.crash_hi_ns),
            (1_000_000_000, 1_000_000_000)
        );
        assert_eq!(m.processes[2].faults, Faults::default());
    }

    #[test]
    fn a_bare_number_is_a_valid_latency() {
        let m = parse("net-latency: 5\nhosts: [{name: a, processes: [p]}]\n").unwrap();
        assert_eq!(m.net_latency.as_deref(), Some("5"));
    }

    #[test]
    fn the_switch_cost_is_read_as_given() {
        let m = parse("switch-cost: 50us\nhosts: [{name: a, processes: [p]}]\n").unwrap();
        assert_eq!(m.switch_cost.as_deref(), Some("50us"));
    }

    #[test]
    fn pauses() {
        let m =
            parse("pause: {chance: 1/200, for: 10us..100ms}\nhosts: [{name: a, processes: [p]}]")
                .unwrap();
        assert_eq!(m.pause_chance.as_deref(), Some("1/200"));
        assert_eq!(m.pause_for.as_deref(), Some("10us..100ms"));
        let m = parse("pause: off\nhosts: [{name: a, processes: [p]}]").unwrap();
        assert_eq!(m.pause_chance.as_deref(), Some("off"));
        assert_eq!(m.pause_for, None);
        assert_eq!(parse_chance("off"), Some((0, 1)));
        assert_eq!(parse_chance("3/1000"), Some((3, 1000)));
        assert_eq!(parse_chance("2/1"), None);
        assert_eq!(parse_chance("1/0"), None);
        assert_eq!(parse_pause_for("1ms"), Some((1_000_000, 1_000_000)));
        assert_eq!(parse_pause_for("0..1ms"), None);
        let err = |text: &str| parse(text).unwrap_err();
        assert!(err("pause: sometimes\nhosts: [{name: a, processes: [p]}]").contains("bad chance"));
        assert!(
            err("pause: {chance: 1/2, for: 0}\nhosts: [{name: a, processes: [p]}]")
                .contains("bad for")
        );
        assert!(
            err("pause: {chance: 1/2, often: 1}\nhosts: [{name: a, processes: [p]}]")
                .contains("run file")
        );
    }

    #[test]
    fn pct() {
        let m = parse(
            "scheduler: pct\npct: {depth: 4, horizon: 10s, starve: 5ms}\nhosts: [{name: a, processes: [p]}]",
        )
        .unwrap();
        assert_eq!(m.scheduler.as_deref(), Some("pct"));
        assert_eq!(m.pct_depth, Some(4));
        assert_eq!(m.pct_horizon.as_deref(), Some("10s"));
        assert_eq!(m.pct_starve.as_deref(), Some("5ms"));
        let err = |text: &str| parse(text).unwrap_err();
        let hosts = "\nhosts: [{name: a, processes: [p]}]";
        assert!(err(&format!("scheduler: fifo{hosts}")).contains("random or pct"));
        assert!(err(&format!("pct: {{depth: 0}}{hosts}")).contains("depth 0"));
        assert!(err(&format!("pct: {{depth: 17}}{hosts}")).contains("depth 17"));
        assert!(err(&format!("pct: {{horizon: 0}}{hosts}")).contains("bad horizon"));
        assert!(err(&format!("pct: {{starve: soon}}{hosts}")).contains("bad starve"));
        assert!(err(&format!("pct: {{width: 3}}{hosts}")).contains("run file"));
    }

    #[test]
    fn shutdowns() {
        let m = parse(
            "hosts:
  - name: a
    processes:
      - argv: [server]
        restart: on-failure
        shutdown: { every: 2s..10s, times: 3, signal: INT, grace: 5s }
      - argv: [other]
        shutdown: { every: 1s }
",
        )
        .unwrap();
        let f = m.processes[0].faults;
        assert_eq!(
            (f.shutdown_lo_ns, f.shutdown_hi_ns),
            (2_000_000_000, 10_000_000_000)
        );
        assert_eq!(f.shutdowns_left, 3);
        assert_eq!(i32::from(f.shutdown_signal), libc::SIGINT);
        assert_eq!(f.grace_ns, 5_000_000_000);
        assert_eq!(f.crash_hi_ns, 0);
        let f = m.processes[1].faults;
        assert_eq!(i32::from(f.shutdown_signal), libc::SIGTERM);
        assert_eq!(f.grace_ns, DEFAULT_GRACE_NS);
        assert_eq!(f.shutdowns_left, shared::NO_LIMIT);
        assert_eq!(f.restart, shared::RESTART_NEVER);
        let err = |sd: &str| {
            parse(&format!(
                "hosts: [{{name: a, processes: [{{argv: [p], shutdown: {sd}}}]}}]"
            ))
            .unwrap_err()
        };
        assert!(err("{every: 0}").contains("bad shutdown every"));
        assert!(err("{every: 1s, signal: KILL}").contains("shutdown signal KILL"));
        assert!(err("{every: 1s, grace: 0}").contains("bad shutdown grace"));
        assert!(err("{every: 1s, after: 2s}").contains("run file"));
        assert!(parse(
            "hosts: [{name: a, processes: [{argv: [p], shutdown: {every: 1s, signal: SIGHUP}}]}]"
        )
        .is_ok());
    }

    #[test]
    fn errors_say_what_is_wrong() {
        let err = |text: &str| parse(text).unwrap_err();
        assert!(err("hosts: [{name: a}]").contains("no processes"));
        let process = |p: &str| err(&format!("hosts: [{{name: a, processes: [{p}]}}]"));
        assert!(process("{argv: [p], deamon: true}").contains("unknown field `deamon`"));
        assert!(process("{argv: [p], crash: {evry: 1s}}").contains("unknown field `evry`"));
        assert!(process("{argv: [p], crash: {times: 2}}").contains("missing field `every`"));
        assert!(process("{env: {A: b}}").contains("missing field `argv`"));
        assert!(process("7").contains("a line, a list"));
        assert!(
            err("pause: {chanse: 1/2}\nhosts: [{name: a, processes: [p]}]")
                .contains("unknown field `chanse`")
        );
        assert!(
            err("hosts: [{name: a, processes: [p]}, {name: a, processes: [p]}]")
                .contains("declared twice")
        );
        assert!(err("hosts: [{name: 'a b', processes: [p]}]").contains("host name"));
        assert!(err("hosts: [{name: '..', processes: [p]}]").contains("host name"));
        assert!(err("hosts: [{name: a, processes: [[]]}]").contains("no program"));
        assert!(err("hosts: [{name: a, procs: [p]}]").contains("procs"));
        assert!(err("hosts: [{name: a, root: /x, processes: [p]}]").contains("root"));
        assert!(err("sed: 1\nhosts: [{name: a, processes: [p]}]").contains("sed"));
        assert!(err("processes: [p]").contains("hosts"));
        assert!(err("hosts: [{name: a, processes: [p]").contains("run file"));
        assert!(
            err("hosts: [{name: a, processes: [{argv: [p], deamon: true}]}]").contains("run file")
        );
        assert!(
            err("env: {REWRITE_PROC: 3}\nhosts: [{name: a, processes: [p]}]").contains("reserved")
        );
        assert!(
            err("pass-env: [DYLD_INSERT_LIBRARIES]\nhosts: [{name: a, processes: [p]}]")
                .contains("reserved")
        );
        assert!(
            err("hosts: [{name: a, processes: [{argv: [p], env: {REWRITE_SEED: 1}}]}]")
                .contains("reserved")
        );
        assert!(err("hosts: [{name: stdout.0, processes: [p]}]").contains("host name"));
        assert!(
            err("hosts: [{name: a, processes: [{argv: [p], restart: sometimes}]}]")
                .contains("expected never")
        );
        assert!(
            err("hosts: [{name: a, processes: [{argv: [p], restart-delay: 1s}]}]")
                .contains("need a restart policy")
        );
        assert!(err(
            "hosts: [{name: a, processes: [{argv: [p], restart: always, restart-delay: 0}]}]"
        )
        .contains("bad restart-delay"));
        assert!(
            err("hosts: [{name: a, processes: [{argv: [p], crash: {every: 9s..1s}}]}]")
                .contains("bad crash every")
        );
        // Longer than a year: the virtual clock could not hold many of these
        assert!(
            err("hosts: [{name: a, processes: [{argv: [p], crash: {every: 99999999999s}}]}]")
                .contains("bad crash every")
        );
        assert!(
            err("hosts: [{name: a, processes: [{argv: [p], crash: {evry: 1s}}]}]")
                .contains("run file")
        );
        assert!(
            err("hosts: [{name: Web, processes: [p]}, {name: web, processes: [p]}]")
                .contains("declared twice")
        );
    }
}
