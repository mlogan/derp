//! Rewriting to files: signing, and the cache next to each program that
//! lets the launcher rewrite a spawned child's binary on demand.

use std::path::{Path, PathBuf};

use crate::macho::{self, MachO};
use crate::rewrite::{self as rw, Options};

pub type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

pub fn write_exe(path: &Path, image: &[u8]) -> Fallible<()> {
    std::fs::write(path, image)?;
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    macho::adhoc_sign(path)?;
    Ok(())
}

pub fn read_macho(path: &Path) -> Fallible<MachO> {
    Ok(MachO::parse(std::fs::read(path)?)?)
}

pub fn rewrite_file(input: &Path, output: &Path, opts: &Options) -> Fallible<rw::Stats> {
    let r = rw::rewrite(&read_macho(input)?, opts)?;
    write_exe(output, &r.image)?;
    Ok(r.stats)
}

/// Rewrite into a cache file next to the program, keyed by options and
/// the input's modification time.
pub fn cached_rewrite(input: &Path, opts: &Options) -> Fallible<PathBuf> {
    // A guest that starts another copy of itself names its own, already
    // rewritten, file.
    if read_macho(input)?
        .segment(macho::STUB_TEXT_SEGMENT)
        .is_some()
    {
        return Ok(input.to_path_buf());
    }
    let mtime = std::fs::metadata(input)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let name = format!(
        "{}.rw2-{}-{}of{}-{mtime}",
        input.file_name().unwrap_or_default().to_string_lossy(),
        opts.seed,
        opts.mem_rate.0,
        opts.mem_rate.1
    );
    let out = input.with_file_name(name);
    if !out.exists() {
        // Rename into place: another run may be rewriting the same program.
        let tmp = input.with_file_name(format!(
            ".{}.rw-tmp-{}",
            input.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id()
        ));
        let stats = rewrite_file(input, &tmp, opts)?;
        std::fs::rename(&tmp, &out)?;
        eprintln!(
            "rewrite: {} sites hooked -> {}",
            stats.branch_sites + stats.call_sites + stats.mem_sites,
            out.display()
        );
    }
    Ok(out)
}
