//! A directory per virtual host, made fresh for every run. It is where the
//! host's processes start, and the supervisor holds their path names to it
//! (`supervisor/src/hostfs.rs`). Not a sandbox: it guards against
//! configurations that would let hosts share files by accident.

use std::io;
use std::path::{Path, PathBuf};

use crate::manifest::Host;

/// Create `<scratch>/<name>` for every host, with a `tmp` inside, and copy
/// the host's input files in. `scratch` must exist, be empty of host
/// directories, and be canonical; `base` is what the run file's relative
/// paths are relative to.
pub fn prepare(scratch: &Path, base: &Path, hosts: &[Host]) -> io::Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    for host in hosts {
        let root = scratch.join(&host.name);
        std::fs::create_dir(&root)?;
        std::fs::create_dir(root.join("tmp"))?;
        for file in &host.files {
            let from = base.join(file);
            let name = from.file_name().ok_or_else(|| {
                io::Error::other(format!("host {}: {file} has no file name", host.name))
            })?;
            copy_tree(&from, &root.join(name))
                .map_err(|e| io::Error::other(format!("host {}: {file}: {e}", host.name)))?;
        }
        roots.push(root);
    }
    Ok(roots)
}

/// Modes come along: a database refuses a data directory others may read.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let meta = std::fs::metadata(from)?;
    if meta.is_dir() {
        std::fs::create_dir(to)?;
        std::fs::set_permissions(to, meta.permissions())?;
        let mut entries: Vec<_> = std::fs::read_dir(from)?.collect::<Result<_, _>>()?;
        // Creation order shows in directory listings; keep it repeatable
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(from, to)?;
    }
    Ok(())
}
