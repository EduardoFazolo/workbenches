//! Copy a whole folder as cheaply as the filesystem allows.
//!
//! - macOS/APFS: one `clonefile(2)` call clones the entire tree copy-on-write.
//! - Linux (btrfs, XFS) and Windows (ReFS, Dev Drive; untested): walk the tree and
//!   reflink each file.
//! - Anything else can only do a real copy, which we refuse unless asked
//!   (`--copy`), because a silent 10 GB copy is worse than an error.

use crate::util::human_bytes;
use anyhow::{Context, Result, bail};
use filetime::FileTime;
use rayon::prelude::*;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use walkdir::WalkDir;

#[derive(PartialEq, Eq)]
pub enum Mode {
    /// Copy-on-write: near-zero disk until files change.
    Clone,
    /// Real byte-for-byte copy.
    Copy,
}

pub fn clone_tree(src: &Path, dst: &Path, allow_copy: bool) -> Result<Mode> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(target_os = "macos")]
    {
        match clonefile_dir(src, dst) {
            Ok(()) => return Ok(Mode::Clone),
            // Different volume, or not APFS: fall back to the walking path.
            Err(e) if matches!(e.raw_os_error(), Some(libc::EXDEV) | Some(libc::ENOTSUP) | Some(libc::EOPNOTSUPP)) => {}
            Err(e) => return Err(e).with_context(|| format!("cloning {} to {}", src.display(), dst.display())),
        }
    }
    walk_clone(src, dst, allow_copy)
}

#[cfg(target_os = "macos")]
fn clonefile_dir(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let s = CString::new(src.as_os_str().as_bytes())?;
    let d = CString::new(dst.as_os_str().as_bytes())?;
    // SAFETY: both are valid NUL-terminated paths; flags 0 = default behaviour.
    let r = unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) };
    if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

fn walk_clone(src: &Path, dst: &Path, allow_copy: bool) -> Result<Mode> {
    let entries: Vec<walkdir::DirEntry> = WalkDir::new(src)
        .follow_links(false)
        .into_iter()
        .collect::<Result<_, _>>()
        .with_context(|| format!("reading {}", src.display()))?;
    fs::create_dir(dst).with_context(|| format!("creating {}", dst.display()))?;

    // Probe once whether this filesystem pair can reflink at all.
    let cow = match entries.iter().find(|e| e.file_type().is_file()) {
        Some(e) => {
            let probe = dst.join(".wb-probe");
            let ok = reflink_copy::reflink(e.path(), &probe).is_ok();
            let _ = fs::remove_file(&probe);
            ok
        }
        None => true,
    };
    if !cow && !allow_copy {
        let bytes: u64 =
            entries.iter().filter(|e| e.file_type().is_file()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum();
        let _ = fs::remove_dir(dst);
        bail!(
            "this disk can't make free copy-on-write copies, so this one would use {} of real space.\n\
             Re-run with --copy to do it anyway, or set WB_HOME to a folder on the same disk\n\
             if that disk supports clones (APFS, btrfs, XFS, ReFS / Dev Drive).",
            human_bytes(bytes)
        );
    }

    for e in &entries {
        if e.file_type().is_dir() {
            fs::create_dir_all(dst.join(e.path().strip_prefix(src)?))?;
        }
    }

    let link_failures = AtomicUsize::new(0);
    entries.par_iter().try_for_each(|e| -> Result<()> {
        let to = dst.join(e.path().strip_prefix(src)?);
        let ft = e.file_type();
        if ft.is_file() {
            let cloned = cow && reflink_copy::reflink(e.path(), &to).is_ok();
            if !cloned {
                fs::copy(e.path(), &to).with_context(|| format!("copying {}", e.path().display()))?;
            }
            let md = e.metadata()?;
            let _ = fs::set_permissions(&to, md.permissions());
            // Keep mtimes: build caches and git's index both key on them.
            let _ = filetime::set_file_times(
                &to,
                FileTime::from_last_access_time(&md),
                FileTime::from_last_modification_time(&md),
            );
        } else if ft.is_symlink() {
            let target = fs::read_link(e.path())?;
            if make_symlink(&target, &to).is_err() {
                link_failures.fetch_add(1, Ordering::Relaxed);
            }
        }
        // Sockets, fifos, devices: skipped on purpose.
        Ok(())
    })?;

    let n = link_failures.into_inner();
    if n > 0 {
        eprintln!("  ! {n} symlinks could not be recreated (on Windows, enable Developer Mode to allow symlinks)");
    }
    Ok(if cow { Mode::Clone } else { Mode::Copy })
}

pub fn make_symlink(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        let resolved = if target.is_absolute() {
            target.to_path_buf()
        } else {
            link.parent().map(|p| p.join(target)).unwrap_or_else(|| target.to_path_buf())
        };
        if resolved.is_dir() {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        }
    }
}
