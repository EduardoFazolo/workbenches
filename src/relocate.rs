//! Some tools write the repo's absolute path into files they generate. Copied
//! as they are, those files make the copy run or write into the original. `wb`
//! fixes these places, and only these:
//!
//! - Python virtualenvs (any folder with a `pyvenv.cfg`): `pyvenv.cfg`, the
//!   scripts in `bin/`, and the `.pth` / `.egg-link` files in `site-packages`
//! - `node_modules/.bin` (package manager shims)
//! - `.git/hooks`
//!
//! It also deletes untracked `*.pid` files and `.next/dev/lock`, left by servers
//! running in the original, so the copy's server doesn't think it's running.
//! Anything else is for the repo's `.wb/setup`.

use crate::gitfix;
use crate::util::canonical;
use anyhow::{Context, Result};
use filetime::FileTime;
use memchr::memmem;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const MAX_TEXT: u64 = 1 << 20;

#[derive(Default)]
pub struct Stats {
    pub fixed: usize,
    pub removed: usize,
    /// Files that needed fixing but couldn't be, with the reason.
    pub failed: Vec<String>,
}

pub fn relocate(src: &Path, dst: &Path) -> Result<Stats> {
    let pairs = pairs(src, dst);
    let mut to_fix = Vec::new();
    let mut leftovers = Vec::new();
    let mut walk = WalkDir::new(dst).follow_links(false).into_iter();
    while let Some(e) = walk.next() {
        let e = e.with_context(|| format!("reading {}", dst.display()))?;
        let path = e.path();
        if e.file_type().is_dir() {
            let known = if e.file_name() == ".git" {
                Some(files_in(&path.join("hooks")))
            } else if e.file_name() == "node_modules" {
                Some(files_in(&path.join(".bin")))
            } else if path.join("pyvenv.cfg").is_file() {
                Some(venv_files(path))
            } else {
                None
            };
            if let Some(files) = known {
                to_fix.extend(files);
                walk.skip_current_dir();
            }
        } else if e.file_name().to_string_lossy().ends_with(".pid") || path.ends_with(".next/dev/lock") {
            leftovers.push(path.to_path_buf());
        }
    }

    let mut stats = Stats::default();
    for path in to_fix {
        match rewrite(&path, &pairs) {
            Ok(true) => stats.fixed += 1,
            Ok(false) => {}
            Err(e) => stats.failed.push(format!("{}: {e}", path.display())),
        }
    }
    for path in untracked(dst, leftovers)? {
        match fs::remove_file(&path) {
            Ok(()) => stats.removed += 1,
            Err(e) => stats.failed.push(format!("{}: {e}", path.display())),
        }
    }
    Ok(stats)
}

/// Regular files directly in `dir` (symlinks are left alone).
fn files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_file())).map(|e| e.path()).collect()
}

fn venv_files(venv: &Path) -> Vec<PathBuf> {
    let mut out = vec![venv.join("pyvenv.cfg")];
    out.extend(files_in(&venv.join("bin")));
    out.extend(files_in(&venv.join("Scripts")));
    for lib in ["lib", "lib64"] {
        for python in fs::read_dir(venv.join(lib)).into_iter().flatten().flatten() {
            let site = python.path().join("site-packages");
            out.extend(
                files_in(&site).into_iter().filter(|p| p.extension().is_some_and(|x| x == "pth" || x == "egg-link")),
            );
        }
    }
    out
}

/// The paths among `paths` git doesn't track: a committed pid file is left alone.
fn untracked(repo: &Path, paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    if paths.is_empty() {
        return Ok(paths);
    }
    let rel: Vec<String> =
        paths.iter().filter_map(|p| p.strip_prefix(repo).ok()).map(|p| p.to_string_lossy().into_owned()).collect();
    let mut args = vec!["ls-files", "-z", "--"];
    args.extend(rel.iter().map(String::as_str));
    let tracked = gitfix::run(repo, &args)?;
    let tracked: Vec<&str> = tracked.split('\0').collect();
    Ok(paths
        .into_iter()
        .filter(|p| !p.strip_prefix(repo).is_ok_and(|r| tracked.contains(&&*r.to_string_lossy())))
        .collect())
}

/// Every spelling of `<original>/` a tool might have written, paired with `<copy>/`.
/// Matching the trailing `/` means only paths inside the original match, never a
/// sibling like `app data` next to `app`.
fn pairs(src: &Path, dst: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let slash = |p: &Path| format!("{}/", p.to_string_lossy().trim_end_matches('/'));
    let new = slash(&canonical(dst));
    let mut olds = vec![slash(src), slash(&canonical(src))];
    // macOS: /var, /tmp and /etc are symlinks into /private, and tools record
    // whichever spelling they were handed.
    for o in olds.clone() {
        if let Some(rest) = o.strip_prefix("/private")
            && ["/var/", "/tmp/", "/etc/"].iter().any(|d| rest.starts_with(d))
        {
            olds.push(rest.to_string());
        }
    }
    olds.sort_by_key(|o| std::cmp::Reverse(o.len()));
    olds.dedup();
    olds.into_iter().map(|o| (o.into_bytes(), new.clone().into_bytes())).collect()
}

/// Replace every occurrence that starts a path (so `/var/x/` isn't matched
/// inside `/private/var/x/`, which has its own pair).
fn replace_all(data: &[u8], pairs: &[(Vec<u8>, Vec<u8>)]) -> Option<Vec<u8>> {
    let mut out = data.to_vec();
    let mut changed = false;
    for (old, new) in pairs {
        let starts =
            |d: &[u8], pos: usize| pos == 0 || !(d[pos - 1].is_ascii_alphanumeric() || b"_-.".contains(&d[pos - 1]));
        let hits: Vec<usize> = memmem::find_iter(&out, old).filter(|&pos| starts(&out, pos)).collect();
        if hits.is_empty() {
            continue;
        }
        let mut next = Vec::with_capacity(out.len());
        let mut cur = 0;
        for pos in hits {
            next.extend_from_slice(&out[cur..pos]);
            next.extend_from_slice(new);
            cur = pos + old.len();
        }
        next.extend_from_slice(&out[cur..]);
        out = next;
        changed = true;
    }
    changed.then_some(out)
}

/// Rewrites a small text file in place, keeping its mtime and permissions.
/// Returns whether it changed.
fn rewrite(path: &Path, pairs: &[(Vec<u8>, Vec<u8>)]) -> io::Result<bool> {
    let md = fs::metadata(path)?;
    if md.len() == 0 || md.len() > MAX_TEXT {
        return Ok(false);
    }
    let data = fs::read(path)?;
    if memchr::memchr(0, &data).is_some() {
        return Ok(false); // binary
    }
    let Some(new) = replace_all(&data, pairs) else { return Ok(false) };
    let perms = md.permissions();
    if perms.readonly() {
        let mut writable = perms.clone();
        #[allow(clippy::permissions_set_readonly_false)]
        writable.set_readonly(false);
        fs::set_permissions(path, writable)?;
    }
    let written = fs::write(path, new);
    if perms.readonly() {
        fs::set_permissions(path, perms)?;
    }
    written?;
    filetime::set_file_mtime(path, FileTime::from_last_modification_time(&md))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> Vec<(Vec<u8>, Vec<u8>)> {
        vec![(b"/src/app/".to_vec(), b"/wb/app/x/".to_vec())]
    }

    #[test]
    fn replaces_paths_inside_the_original_only() {
        let r = replace_all(b"#!/src/app/.venv/bin/python\nPATH=/src/app/bin:/src/app/x\n", &p()).unwrap();
        assert_eq!(r, b"#!/wb/app/x/.venv/bin/python\nPATH=/wb/app/x/bin:/wb/app/x/x\n");
        assert!(replace_all(b"/src/app-old/x", &p()).is_none());
        assert!(replace_all(b"/src/app data/x", &p()).is_none());
        assert!(replace_all(b"/private/src/app/x", &p()).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn knows_both_spellings_of_private_paths() {
        let olds: Vec<Vec<u8>> =
            pairs(Path::new("/private/tmp/app"), Path::new("/x")).into_iter().map(|(o, _)| o).collect();
        assert!(olds.contains(&b"/private/tmp/app/".to_vec()));
        assert!(olds.contains(&b"/tmp/app/".to_vec()));
    }
}
