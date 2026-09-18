//! Run manifest: which processes start on which virtual host.
//!
//! ```text
//! host alpha
//!     ./server --port 80
//! host beta
//!     ./client alpha "hello world"
//! ```
//!
//! A `host NAME` line opens a host; each indented line under it is one
//! initial process whose tokens become the guest's `argv` verbatim.
//! Processes start in file order. `#` starts a comment line.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    /// Index into `Manifest::hosts`
    pub host: u32,
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub hosts: Vec<String>,
    pub processes: Vec<Process>,
}

/// Split a line into tokens; double quotes group, and inside them `\"` and
/// `\\` are escapes.
fn tokenize(line: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_token = true;
                loop {
                    match chars.next() {
                        None => return Err("unterminated quote".into()),
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(e @ ('"' | '\\')) => cur.push(e),
                            _ => return Err("bad escape in quotes".into()),
                        },
                        Some(other) => cur.push(other),
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_token {
                    tokens.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            c => {
                in_token = true;
                cur.push(c);
            }
        }
    }
    if in_token {
        tokens.push(cur);
    }
    Ok(tokens)
}

pub fn parse(text: &str) -> Result<Manifest, String> {
    let mut m = Manifest::default();
    for (i, line) in text.lines().enumerate() {
        let at = |e: String| format!("manifest line {}: {e}", i + 1);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indented = line.starts_with([' ', '\t']);
        let tokens = tokenize(trimmed).map_err(at)?;
        if indented {
            if m.hosts.is_empty() {
                return Err(at("process before any `host` line".into()));
            }
            m.processes.push(Process {
                host: m.hosts.len() as u32 - 1,
                argv: tokens,
            });
        } else {
            match tokens.as_slice() {
                [kw, name] if kw == "host" => {
                    if m.hosts.contains(name) {
                        return Err(at(format!("host {name} declared twice")));
                    }
                    m.hosts.push(name.clone());
                }
                _ => {
                    return Err(at(
                        "expected `host NAME` (process lines are indented)".into()
                    ))
                }
            }
        }
    }
    if m.processes.is_empty() {
        return Err("manifest lists no processes".into());
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_and_processes_in_order() {
        let m = parse(
            "# two hosts\nhost a\n  ./srv --port 80\n\nhost b\n\t./cli a \"hello world\" \"q\\\"x\"\n  ./cli -- --seed 3\n",
        )
        .unwrap();
        assert_eq!(m.hosts, ["a", "b"]);
        assert_eq!(m.processes.len(), 3);
        assert_eq!(m.processes[0].host, 0);
        assert_eq!(m.processes[0].argv, ["./srv", "--port", "80"]);
        assert_eq!(m.processes[1].host, 1);
        assert_eq!(m.processes[1].argv, ["./cli", "a", "hello world", "q\"x"]);
        assert_eq!(m.processes[2].argv, ["./cli", "--", "--seed", "3"]);
    }

    #[test]
    fn empty_quotes_are_an_argument() {
        assert_eq!(tokenize("a \"\" b").unwrap(), ["a", "", "b"]);
    }

    #[test]
    fn errors_name_the_line() {
        assert!(parse("  ./p\n").unwrap_err().contains("line 1"));
        assert!(parse("host a\nhost a\n").unwrap_err().contains("line 2"));
        assert!(parse("host a\n  ./p \"x\n")
            .unwrap_err()
            .contains("unterminated"));
        assert!(parse("hots a\n").unwrap_err().contains("line 1"));
        assert!(parse("host a\n").unwrap_err().contains("no processes"));
    }
}
