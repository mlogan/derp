//! The run file: which processes start on which virtual host, and the
//! settings of the run. YAML:
//!
//! ```yaml
//! seed: 7                  # optional; the command line overrides these four
//! quantum: 1000..10000
//! mem-hook-rate: 1/16
//! net-latency: 5ms
//! allow:                   # extra paths every host may touch
//!   - /opt/site-content
//! hosts:                   # in order: 10.0.0.1, 10.0.0.2, ...
//!   - name: alpha
//!     files: [site/index.html, site/img]   # copied into the host's directory
//!     processes:
//!       - [server, --port, 8080]        # argv verbatim
//!       - client alpha 8080             # or a line, split on whitespace
//!       - argv: [worker, "two words"]   # or a map, with an environment
//!         env: { MODE: fast }
//! ```
//!
//! Processes start in file order. `argv[0]` names the program, relative to
//! the run file, and reaches the guest as written. A host's directory is
//! never named here: the launcher makes a fresh one for every run.

use std::collections::BTreeMap;

use serde::Deserialize;

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

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawProcess {
    /// Split on whitespace; use the list form for arguments with spaces
    Line(String),
    Argv(Vec<Scalar>),
    Full {
        argv: Vec<Scalar>,
        #[serde(default)]
        env: BTreeMap<String, Scalar>,
    },
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
    #[serde(default)]
    allow: Vec<String>,
    hosts: Vec<RawHost>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    /// Index into `Manifest::hosts`
    pub host: u32,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
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
    pub allow: Vec<String>,
}

/// A host's name is also the name of its directory.
fn valid_host_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

pub fn parse(text: &str) -> Result<Manifest, String> {
    let raw: RawRun = serde_yaml::from_str(text).map_err(|e| format!("run file: {e}"))?;
    let mut m = Manifest {
        seed: raw.seed,
        quantum: raw.quantum,
        mem_hook_rate: raw.mem_hook_rate.as_ref().map(Scalar::text),
        net_latency: raw.net_latency.as_ref().map(Scalar::text),
        allow: raw.allow,
        ..Manifest::default()
    };
    for host in raw.hosts {
        if !valid_host_name(&host.name) {
            return Err(format!(
                "run file: host name {:?} must be letters, digits, '-' or '.'",
                host.name
            ));
        }
        if m.hosts.iter().any(|h| h.name == host.name) {
            return Err(format!("run file: host {} declared twice", host.name));
        }
        let index = m.hosts.len() as u32;
        for p in host.processes {
            let (argv, env): (Vec<String>, Vec<(String, String)>) = match p {
                RawProcess::Line(line) => (
                    line.split_whitespace().map(str::to_string).collect(),
                    Vec::new(),
                ),
                RawProcess::Argv(argv) => (argv.iter().map(Scalar::text).collect(), Vec::new()),
                RawProcess::Full { argv, env } => (
                    argv.iter().map(Scalar::text).collect(),
                    env.iter().map(|(k, v)| (k.clone(), v.text())).collect(),
                ),
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
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn a_bare_number_is_a_valid_latency() {
        let m = parse("net-latency: 5\nhosts: [{name: a, processes: [p]}]\n").unwrap();
        assert_eq!(m.net_latency.as_deref(), Some("5"));
    }

    #[test]
    fn errors_say_what_is_wrong() {
        let err = |text: &str| parse(text).unwrap_err();
        assert!(err("hosts: [{name: a}]").contains("no processes"));
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
    }
}
