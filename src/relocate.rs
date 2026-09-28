//! After copying, some files still hold the original folder's absolute path
//! (venv script shebangs, pnpm shims, Bundler config, editable installs, git
//! hooks...). Left alone, the copy silently runs or writes into the original.
//!
//! One generic rule instead of per-language knowledge: in small, untracked
//! text files, replace the old folder path with the new one. Same idea as
//! conda-pack's prefix rewriting. Committed files (in any repo inside the copy,
//! submodules included) are never touched, only reported.

use crate::clone::make_symlink;
use crate::gitfix;
use crate::util::{canonical, rel_key};
use anyhow::{Context, Result};
use filetime::FileTime;
use memchr::memmem;
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use walkdir::WalkDir;

const MAX_TEXT: u64 = 1 << 20;

#[derive(Default)]
pub struct Stats {
    pub files: usize,
    pub links: usize,
    pub removed: usize,
    /// Tracked files that mention the original path; reported, not changed.
    pub tracked_hits: Vec<String>,
    /// Files that needed fixing but couldn't be, with the reason.
    pub failed: Vec<String>,
    /// Repos (relative to the copy) git couldn't list; left entirely as copied.
    pub unlisted: Vec<String>,
}

/// What to rewrite: every spelling of the original's path paired with the
/// copy's, plus what can follow the original's name without being it.
struct Paths {
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
    /// With `app data` next to `app`, " data": text that continues into a
    /// sibling's name is that sibling, not the original.
    sibling_tails: Vec<Vec<u8>>,
}

fn paths(src: &Path, dst: &Path) -> Paths {
    let src = canonical(src);
    let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let sibling_tails = src
        .parent()
        .and_then(|p| fs::read_dir(p).ok())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().strip_prefix(name.as_str()).map(|t| t.as_bytes().to_vec()))
        .filter(|tail| !tail.is_empty())
        .collect();
    Paths { pairs: pairs(&src, dst), sibling_tails }
}

/// Every spelling of the old path we might find, paired with the new one.
fn pairs(src: &Path, dst: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let trim = |p: &Path| p.to_string_lossy().trim_end_matches(['/', '\\']).to_string();
    let new = trim(&canonical(dst));
    let mut olds = vec![trim(src), trim(&canonical(src))];
    // macOS: /var, /tmp and /etc are symlinks into /private, and tools record
    // whichever spelling they were handed.
    for o in olds.clone() {
        if let Some(rest) = o.strip_prefix("/private")
            && (rest.starts_with("/var/") || rest.starts_with("/tmp/") || rest.starts_with("/etc/"))
        {
            olds.push(rest.to_string());
        }
    }
    let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut add = |a: String, b: String| {
        if !a.is_empty() && !out.iter().any(|(x, _)| x == a.as_bytes()) {
            out.push((a.into_bytes(), b.into_bytes()));
        }
    };
    for old in olds {
        if cfg!(windows) {
            add(old.replace('\\', "/"), new.replace('\\', "/"));
            add(old.replace('\\', "\\\\"), new.replace('\\', "\\\\")); // JSON-escaped
        }
        add(old, new.clone());
    }
    out.sort_by_key(|(a, _)| std::cmp::Reverse(a.len()));
    out
}

/// Bytes that continue a file name. Non-ASCII counts: `/src/app` isn't in `/src/appé`.
fn is_path_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.') || b >= 0x80
}

/// Replace whole-path occurrences only: `/src/app` must not match inside
/// `/src/app-old`, `/private/src/app` or a sibling folder `/src/app data`.
fn replace_all(data: &[u8], paths: &Paths) -> Option<Vec<u8>> {
    let pairs = &paths.pairs;
    let mut hits: Vec<(usize, usize, usize)> = Vec::new();
    for (i, (old, _)) in pairs.iter().enumerate() {
        for pos in memmem::find_iter(data, old) {
            let end = pos + old.len();
            let starts_clean = pos == 0 || !is_path_char(data[pos - 1]);
            let ends_clean = (end == data.len() || !is_path_char(data[end]))
                && !paths.sibling_tails.iter().any(|tail| data[end..].starts_with(tail));
            if starts_clean && ends_clean {
                hits.push((pos, old.len(), i));
            }
        }
    }
    if hits.is_empty() {
        return None;
    }
    hits.sort_by_key(|&(pos, len, _)| (pos, std::cmp::Reverse(len)));
    let mut out = Vec::with_capacity(data.len() + 64);
    let mut cur = 0;
    for (pos, len, i) in hits {
        if pos < cur {
            continue;
        }
        out.extend_from_slice(&data[cur..pos]);
        out.extend_from_slice(&pairs[i].1);
        cur = pos + len;
    }
    out.extend_from_slice(&data[cur..]);
    Some(out)
}

/// Git internals that never hold paths worth rewriting, and are huge.
fn skip_dir(e: &walkdir::DirEntry) -> bool {
    e.file_type().is_dir()
        && matches!(e.file_name().to_str(), Some("objects" | "lfs" | "logs" | "refs"))
        && e.path().components().any(|c| c.as_os_str() == ".git")
}

pub fn relocate(src: &Path, dst: &Path) -> Result<Stats> {
    let paths = paths(src, dst);
    let mut entries = Vec::new();
    let mut repos = Vec::new();
    for e in WalkDir::new(dst).follow_links(false).into_iter().filter_entry(|e| !skip_dir(e)) {
        let e = e.with_context(|| format!("reading {}", dst.display()))?;
        if e.file_name() == ".git"
            && let Some(repo) = e.path().parent()
        {
            repos.push(repo.to_path_buf());
        }
        if !e.file_type().is_dir() {
            entries.push(e);
        }
    }
    // Every repo in the copy, submodules and nested ones included. When git
    // can't list one (a broken vendored .git, say), nothing inside it is touched.
    let mut tracked = HashSet::new();
    let mut unlisted = Vec::new();
    for repo in &repos {
        let prefix = rel_key(dst, repo).unwrap_or_default();
        match gitfix::tracked(repo) {
            Ok(files) => {
                tracked.extend(files.into_iter().map(|f| if prefix.is_empty() { f } else { format!("{prefix}/{f}") }))
            }
            Err(_) => unlisted.push(prefix),
        }
    }
    let off_limits = |key: &str| unlisted.iter().any(|root| root.is_empty() || key.starts_with(&format!("{root}/")));

    let files = AtomicUsize::new(0);
    let links = AtomicUsize::new(0);
    let removed = AtomicUsize::new(0);
    let tracked_hits = Mutex::new(Vec::new());
    let failed = Mutex::new(Vec::new());

    entries.par_iter().for_each(|e| {
        let path = e.path();
        let Some(key) = rel_key(dst, path) else { return };
        if off_limits(&key) {
            return;
        }
        let is_tracked = tracked.contains(&key);
        let ft = e.file_type();
        let result = if ft.is_symlink() {
            relink(path, &paths, is_tracked).map(|hit| match hit {
                Hit::Changed => _ = links.fetch_add(1, Ordering::Relaxed),
                Hit::Tracked => tracked_hits.lock().unwrap().push(key.clone()),
                Hit::None => {}
            })
        } else if !ft.is_file() || is_runtime_leftover(&key) {
            // Sockets of the original's running processes, and its pid/lock files.
            if is_tracked {
                Ok(())
            } else {
                fs::remove_file(path).map(|()| _ = removed.fetch_add(1, Ordering::Relaxed))
            }
        } else {
            rewrite(path, e, &paths, is_tracked).map(|hit| match hit {
                Hit::Changed => _ = files.fetch_add(1, Ordering::Relaxed),
                Hit::Tracked => tracked_hits.lock().unwrap().push(key.clone()),
                Hit::None => {}
            })
        };
        if let Err(err) = result {
            failed.lock().unwrap().push(format!("{key}: {err}"));
        }
    });

    let mut tracked_hits = tracked_hits.into_inner().unwrap();
    tracked_hits.sort();
    let mut failed = failed.into_inner().unwrap();
    failed.sort();
    unlisted.sort();
    Ok(Stats {
        files: files.into_inner(),
        links: links.into_inner(),
        removed: removed.into_inner(),
        tracked_hits,
        failed,
        unlisted,
    })
}

enum Hit {
    None,
    Changed,
    /// Mentions the original, but it's committed: reported, never changed.
    Tracked,
}

/// Point an absolute symlink into the original at the copy, matching whole path
/// components. The new link is made next to the old one and renamed over it, so
/// a failure never leaves the link missing.
fn relink(path: &Path, paths: &Paths, is_tracked: bool) -> io::Result<Hit> {
    let target = fs::read_link(path)?;
    let lossy = |b: &[u8]| PathBuf::from(String::from_utf8_lossy(b).into_owned());
    let Some(new) = paths
        .pairs
        .iter()
        .find_map(|(old, new)| target.strip_prefix(lossy(old)).ok().map(|rest| lossy(new).join(rest)))
    else {
        return Ok(Hit::None);
    };
    if is_tracked {
        return Ok(Hit::Tracked);
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".wb-tmp");
    let tmp = PathBuf::from(tmp);
    make_symlink(&new, &tmp)?;
    fs::rename(&tmp, path).inspect_err(|_| _ = fs::remove_file(&tmp))?;
    Ok(Hit::Changed)
}

/// Replace the original's path in a small text file, keeping its mtime (so
/// build tools don't see a change) and its permissions (even read-only ones).
fn rewrite(path: &Path, e: &walkdir::DirEntry, paths: &Paths, is_tracked: bool) -> io::Result<Hit> {
    let md = e.metadata()?;
    if md.len() == 0 || md.len() > MAX_TEXT {
        return Ok(Hit::None);
    }
    let Some(data) = read_text(path, md.len())? else { return Ok(Hit::None) };
    let Some(new) = replace_all(&data, paths) else { return Ok(Hit::None) };
    if is_tracked {
        return Ok(Hit::Tracked);
    }
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
    Ok(Hit::Changed)
}

/// The whole file, unless it's binary (a NUL byte). Sniffs the first 8 KB
/// before reading the rest, so big binaries cost one small read.
fn read_text(path: &Path, len: u64) -> io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let mut f = fs::File::open(path)?;
    let mut data = vec![0u8; len.min(8192) as usize];
    f.read_exact(&mut data)?;
    if memchr::memchr(0, &data).is_some() {
        return Ok(None);
    }
    if len > 8192 {
        f.read_to_end(&mut data)?;
        if memchr::memchr(0, &data[8192..]).is_some() {
            return Ok(None);
        }
    }
    Ok(Some(data))
}

/// Pid and lock files of processes running in the original. Carried over,
/// they make the copy think a server is already running.
fn is_runtime_leftover(key: &str) -> bool {
    let name = key.rsplit('/').next().unwrap_or(key);
    name.ends_with(".pid") || key.ends_with(".next/dev/lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> Paths {
        Paths { pairs: vec![(b"/src/app".to_vec(), b"/wb/app/x".to_vec())], sibling_tails: vec![b" data".to_vec()] }
    }

    #[test]
    fn replaces_whole_paths_only() {
        let r = replace_all(b"#!/src/app/.venv/bin/python\n", &p()).unwrap();
        assert_eq!(r, b"#!/wb/app/x/.venv/bin/python\n");
        assert!(replace_all(b"/src/app-old/x", &p()).is_none());
        assert!(replace_all(b"/private/src/app/x", &p()).is_none());
        assert_eq!(replace_all(b"\"/src/app\"", &p()).unwrap(), b"\"/wb/app/x\"");
        assert!(replace_all("/src/appé/x".as_bytes(), &p()).is_none(), "non-ASCII continues the name");
        assert!(replace_all(b"/src/app data/x", &p()).is_none(), "a sibling folder isn't the original");
        assert_eq!(replace_all(b"cd /src/app && ls", &p()).unwrap(), b"cd /wb/app/x && ls");
    }

    #[test]
    fn replaces_every_occurrence() {
        let r = replace_all(b"/src/app:/src/app/bin", &p()).unwrap();
        assert_eq!(r, b"/wb/app/x:/wb/app/x/bin");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn knows_both_spellings_of_private_paths() {
        let olds: Vec<Vec<u8>> =
            pairs(Path::new("/private/tmp/app"), Path::new("/x")).into_iter().map(|(o, _)| o).collect();
        assert!(olds.contains(&b"/private/tmp/app".to_vec()));
        assert!(olds.contains(&b"/tmp/app".to_vec()));
    }
}
