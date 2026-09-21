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
    let stats = rewrite_without_symbols(input, output, opts)?;
    link_debug_symbols(input, output);
    Ok(stats)
}

fn rewrite_without_symbols(input: &Path, output: &Path, opts: &Options) -> Fallible<rw::Stats> {
    let r = rw::rewrite(&read_macho(input)?, opts)?;
    write_exe(output, &r.image)?;
    std::fs::write(sites_path(output), rw::sites_to_text(&r.sites))?;
    Ok(r.stats)
}

/// Where the site table of a rewritten file is kept
#[must_use]
pub fn sites_path(rewritten: &Path) -> PathBuf {
    let mut name = rewritten.as_os_str().to_owned();
    name.push(".sites");
    PathBuf::from(name)
}

fn with_dsym_suffix(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".dSYM");
    PathBuf::from(name)
}

/// Make `<output>.dSYM` point at the input's dSYM bundle, if it has one.
/// A debugger looks for the bundle by the executable's file name, and the
/// rewritten file has another name (and, for installed programs, another
/// directory), so source-line breakpoints would stay pending. The bundle
/// itself still fits: rewriting keeps the UUID and patches text in place.
/// Best effort: debugging is not what a run depends on.
fn link_debug_symbols(input: &Path, output: &Path) {
    let Ok(bundle) = std::fs::canonicalize(with_dsym_suffix(input)) else {
        return;
    };
    let link = with_dsym_suffix(output);
    if std::fs::read_link(&link).is_ok_and(|target| target == bundle) {
        return;
    }
    // Only ever replace a link, never a real bundle someone put there
    if std::fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()) {
        let _ = std::fs::remove_file(&link);
    }
    let _ = std::os::unix::fs::symlink(&bundle, &link);
}

/// Where rewritten copies of `input` go: next to it, unless it lives where
/// packages and the system install things. We do not write into Homebrew's
/// Cellar; those copies go to a cache directory named after the program's
/// full path.
fn cache_dir_for(input: &Path) -> Fallible<PathBuf> {
    const INSTALLED: [&str; 6] = [
        "/opt/",
        "/usr/",
        "/bin/",
        "/sbin/",
        "/Applications/",
        "/Library/",
    ];
    let full = std::fs::canonicalize(input)?;
    let text = full.to_string_lossy();
    if !INSTALLED.iter().any(|p| text.starts_with(p)) {
        return Ok(full.parent().unwrap_or(Path::new("/")).to_path_buf());
    }
    let mut hash = 0xCBF2_9CE4_8422_2325u64;
    for b in text.bytes() {
        hash = (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3);
    }
    let dir = std::env::temp_dir()
        .join("rewrite-cache")
        .join(format!("{hash:016x}"));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Rewrite into a cache file, keyed by options and the input's
/// modification time.
pub fn cached_rewrite(input: &Path, opts: &Options) -> Fallible<PathBuf> {
    // A guest that starts another copy of itself names its own, already
    // rewritten, file.
    if read_macho(input)?
        .segment(macho::STUB_TEXT_SEGMENT)
        .is_some()
    {
        return Ok(input.to_path_buf());
    }
    // Seconds alone would reuse the rewrite of a program rebuilt within one
    let meta = std::fs::metadata(input)?;
    let mtime = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mtime = format!("{mtime}-{}", meta.len());
    let name = format!(
        "{}{}{}-{}of{}-{mtime}",
        input.file_name().unwrap_or_default().to_string_lossy(),
        crate::shared::CACHE_TAG,
        opts.seed,
        opts.mem_rate.0,
        opts.mem_rate.1
    );
    let dir = cache_dir_for(input)?;
    let out = dir.join(name);
    if !out.exists() {
        // Rename into place: another run may be rewriting the same program.
        static NEXT_TMP: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let tmp = dir.join(format!(
            ".{}.rw-tmp-{}-{}",
            input.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id(),
            NEXT_TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let stats = rewrite_without_symbols(input, &tmp, opts)?;
        // The table first: a rewritten file that exists has one
        std::fs::rename(sites_path(&tmp), sites_path(&out))?;
        std::fs::rename(&tmp, &out)?;
        eprintln!(
            "rewrite: {} sites hooked -> {}",
            stats.branch_sites + stats.call_sites + stats.mem_sites,
            out.display()
        );
    }
    // Also on a cache hit: the program may have gained a dSYM since
    link_debug_symbols(input, &out);
    Ok(out)
}
