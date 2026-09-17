//! Spawn a (rewritten) guest with ASLR disabled and the supervisor dylib
//! injected, and collect the supervisor's end-of-run report.

use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// Not in the libc crate; from `<spawn.h>` on Darwin.
const POSIX_SPAWN_DISABLE_ASLR: libc::c_short = 0x0100;

/// Environment variable naming the fd the supervisor writes its report to.
pub const REPORT_FD_VAR: &str = "REWRITE_REPORT_FD";

pub struct Launch {
    pub exe: PathBuf,
    pub args: Vec<OsString>,
    pub dylib: Option<PathBuf>,
    /// Extra `KEY=VALUE` pairs for the child's environment
    pub env: Vec<(String, String)>,
    pub disable_aslr: bool,
}

#[derive(Debug, Default, Clone)]
pub struct Report {
    pub fields: BTreeMap<String, String>,
}

impl Report {
    fn parse(text: &str) -> Self {
        let mut fields = BTreeMap::new();
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                fields.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        Report { fields }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    pub fn get_u64(&self, key: &str) -> Option<u64> {
        self.get(key)
            .and_then(|v| v.trim_start_matches("0x").parse().ok())
    }
}

#[derive(Debug)]
pub struct Outcome {
    /// Raw wait status; use the helpers below
    pub status: i32,
    pub report: Report,
}

impl Outcome {
    pub fn exit_code(&self) -> Option<i32> {
        libc::WIFEXITED(self.status).then(|| libc::WEXITSTATUS(self.status))
    }

    pub fn signal(&self) -> Option<i32> {
        libc::WIFSIGNALED(self.status).then(|| libc::WTERMSIG(self.status))
    }
}

fn cstring(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).expect("path contains NUL")
}

/// Run the guest to completion. The parent's stdin/stdout/stderr are
/// inherited so the guest's output goes where ours does.
pub fn launch(cfg: &Launch) -> io::Result<Outcome> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    unsafe {
        libc::fcntl(read_fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }

    let mut env: Vec<CString> = std::env::vars_os()
        .filter(|(k, _)| k != "DYLD_INSERT_LIBRARIES" && k != REPORT_FD_VAR)
        .map(|(k, v)| {
            let mut s = k.into_vec();
            s.push(b'=');
            s.extend(v.into_vec());
            CString::new(s).unwrap()
        })
        .collect();
    if let Some(d) = &cfg.dylib {
        let mut s = b"DYLD_INSERT_LIBRARIES=".to_vec();
        s.extend(d.as_os_str().as_bytes());
        env.push(CString::new(s).unwrap());
    }
    env.push(CString::new(format!("{REPORT_FD_VAR}={write_fd}")).unwrap());
    for (k, v) in &cfg.env {
        env.push(CString::new(format!("{k}={v}")).unwrap());
    }

    let exe = cstring(&cfg.exe);
    let mut argv: Vec<CString> = vec![exe.clone()];
    argv.extend(cfg.args.iter().map(|a| CString::new(a.as_bytes()).unwrap()));
    let mut argv_ptrs: Vec<*mut libc::c_char> =
        argv.iter().map(|a| a.as_ptr().cast_mut()).collect();
    argv_ptrs.push(std::ptr::null_mut());
    let mut env_ptrs: Vec<*mut libc::c_char> = env.iter().map(|a| a.as_ptr().cast_mut()).collect();
    env_ptrs.push(std::ptr::null_mut());

    let mut pid: libc::pid_t = 0;
    let rc = unsafe {
        let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
        libc::posix_spawnattr_init(&raw mut attr);
        let flags = if cfg.disable_aslr {
            POSIX_SPAWN_DISABLE_ASLR
        } else {
            0
        };
        libc::posix_spawnattr_setflags(&raw mut attr, flags);
        let rc = libc::posix_spawn(
            &raw mut pid,
            exe.as_ptr(),
            std::ptr::null(),
            &raw const attr,
            argv_ptrs.as_ptr(),
            env_ptrs.as_ptr(),
        );
        libc::posix_spawnattr_destroy(&raw mut attr);
        libc::close(write_fd);
        rc
    };
    if rc != 0 {
        unsafe { libc::close(read_fd) };
        return Err(io::Error::from_raw_os_error(rc));
    }

    let mut text = String::new();
    {
        use std::os::unix::io::FromRawFd;
        let mut f = unsafe { std::fs::File::from_raw_fd(read_fd) };
        f.read_to_string(&mut text)?;
    }
    let mut status = 0;
    if unsafe { libc::waitpid(pid, &raw mut status, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Outcome {
        status,
        report: Report::parse(&text),
    })
}

/// Where the supervisor dylib lives: next to the running `rewrite` binary.
pub fn default_dylib() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let p = dir.join("librewrite_supervisor.dylib");
    p.exists().then_some(p)
}
