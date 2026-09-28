//! Everything git-specific: refusing unsafe sources, and making a copied
//! `.git` a clean, independent repo.

use crate::util::canonical;
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use walkdir::WalkDir;

/// Env vars that would point git at some other repo than the one we name.
const GIT_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

/// Settings that decide who you commit as. `includeIf "gitdir:..."` makes them
/// depend on the folder's path, so a copy in a new place can silently lose them.
const IDENTITY: &[&str] = &[
    "user.name",
    "user.email",
    "user.signingkey",
    "commit.gpgsign",
    "tag.gpgsign",
    "gpg.format",
    "gpg.program",
    "gpg.ssh.program",
    "core.sshcommand",
    "credential.helper",
];

pub fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).stdin(Stdio::null());
    for v in GIT_ENV {
        c.env_remove(v);
    }
    c
}

/// stdout of a git command, or None if it failed.
pub fn out(dir: &Path, args: &[&str]) -> Option<String> {
    let o = git(dir).args(args).stderr(Stdio::null()).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
}

/// stdout of a git command that has to work.
pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let o = git(dir).args(args).output().context("running git (is it installed?)")?;
    if !o.status.success() {
        bail!("git {} in {}: {}", args.join(" "), dir.display(), String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim_end().to_string())
}

pub fn toplevel(dir: &Path) -> Option<PathBuf> {
    out(dir, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

pub fn valid_branch_name(name: &str) -> bool {
    Command::new("git")
        .args(["check-ref-format", "--branch", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Refuse sources where a byte copy would stay wired to the original.
pub fn preflight(repo: &Path) -> Result<()> {
    let dotgit = repo.join(".git");
    let md = fs::symlink_metadata(&dotgit).with_context(|| format!("{} has no .git", repo.display()))?;
    if !md.is_dir() {
        bail!(
            "{} is a linked worktree or a submodule (its .git is a pointer file).\n\
             A copy would share its branch and index with the original. Run wb from the main checkout.",
            repo.display()
        );
    }
    if let Some(wt) = out(repo, &["config", "--local", "--get", "core.worktree"]) {
        bail!(
            "{} sets core.worktree = {wt}, so a copy would still edit the original's files. Unset it first.",
            repo.display()
        );
    }
    if dotgit.join("objects/info/alternates").exists() {
        bail!(
            "{} borrows objects from another repo (made with clone --shared or --reference), so a copy\n\
             would break if that repo moved. Make it self-contained first: git repack -a -d && rm .git/objects/info/alternates",
            repo.display()
        );
    }
    if dotgit.join("index.lock").exists() {
        bail!(
            "a git command is running in {} (.git/index.lock exists).\n\
             Wait for it to finish, or delete .git/index.lock if nothing is running.",
            repo.display()
        );
    }
    let in_progress = [
        ("MERGE_HEAD", "a merge"),
        ("CHERRY_PICK_HEAD", "a cherry-pick"),
        ("REVERT_HEAD", "a revert"),
        ("rebase-merge", "a rebase"),
        ("rebase-apply", "a rebase"),
        ("BISECT_LOG", "a bisect"),
    ];
    for (f, what) in in_progress {
        if dotgit.join(f).exists() {
            bail!(
                "{} is in the middle of {what}. Finish or abort it first, or the copy starts half-done.",
                repo.display()
            );
        }
    }
    Ok(())
}

/// Turn a byte-copied `.git` into a clean repo of its own, on `branch`.
/// Returns notes worth showing the user.
pub fn fix_clone(src_repo: &Path, dst_repo: &Path, branch: &str) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    let gd = dst_repo.join(".git");

    // The original's worktree list: makes branches look "already checked out"
    // and lets `git worktree repair` in the copy rewire the original.
    match remove_dir_all::remove_dir_all(gd.join("worktrees")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).context("removing the original's worktree list from the copy");
        }
        _ => {}
    }

    // Leftovers from git processes running in the original: locks, gc's pid, sockets.
    for e in WalkDir::new(&gd).into_iter().filter_map(Result::ok) {
        let name = e.file_name().to_string_lossy();
        if !e.file_type().is_dir() && (name.ends_with(".lock") || name == "gc.pid" || is_socket(&e)) {
            let _ = fs::remove_file(e.path());
        }
    }

    // Remotes like `../other-repo` now point somewhere else.
    if let Some(list) = out(dst_repo, &["config", "--local", "--get-regexp", r"^remote\..*\.(url|pushurl)$"]) {
        for line in list.lines() {
            let Some((key, url)) = line.split_once(' ') else { continue };
            if ["./", "../", ".\\", "..\\"].iter().any(|p| url.starts_with(p)) {
                let abs = canonical(&src_repo.join(url)).display().to_string();
                if run(dst_repo, &["config", "--local", key, &abs]).is_ok() {
                    notes.push(format!("remote {key} now points at {abs}"));
                }
            }
        }
    }

    // Keep committing as the same person the original commits as.
    let mut carried = Vec::new();
    for key in IDENTITY {
        let Some(want) = out(src_repo, &["config", "--get", key]) else { continue };
        if out(dst_repo, &["config", "--get", key]).as_deref() != Some(want.as_str())
            && run(dst_repo, &["config", "--local", key, &want]).is_ok()
        {
            carried.push(*key);
        }
    }
    if !carried.is_empty() {
        notes.push(format!("kept your git identity from the original ({})", carried.join(", ")));
    }

    // The untracked cache remembers the old folder; rebuild it for the new one.
    if fs::read(gd.join("index")).is_ok_and(|idx| memchr::memmem::find(&idx, b"UNTR").is_some()) {
        let _ = git(dst_repo).args(["update-index", "--untracked-cache"]).output();
    }
    // Every inode changed, so git would re-hash all files on first status.
    let _ = git(dst_repo).args(["update-index", "-q", "--refresh"]).output();

    // An existing branch, local or on a remote, is checked out the way `git
    // switch` does it; anything else is created from HEAD.
    let current = out(dst_repo, &["branch", "--show-current"]).unwrap_or_default();
    if current != branch {
        let exists = out(dst_repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_some()
            || !run(dst_repo, &["branch", "-r", "--list", &format!("*/{branch}")])?.is_empty();
        let args: &[&str] = if exists { &["switch", "-q", branch] } else { &["switch", "-q", "-c", branch] };
        run(dst_repo, args).with_context(|| format!("switching the copy to branch '{branch}'"))?;
    }
    Ok(notes)
}

/// What in `repo` isn't saved anywhere else, for a heads-up before it goes to
/// the trash: changed files, stash entries, and branches with commits no remote
/// has. Best effort: it informs, it never blocks.
pub fn not_saved(repo: &Path) -> Vec<String> {
    let mut notes = Vec::new();
    let Some(status) = out(repo, &["status", "--porcelain"]) else {
        return vec!["(git couldn't read this repo, so check the trash copy yourself)".into()];
    };
    let files: Vec<&str> = status.lines().map(|l| l.get(3..).unwrap_or(l)).collect();
    if !files.is_empty() {
        let more = files.len().saturating_sub(5);
        let more = if more > 0 { format!(" (+{more} more)") } else { String::new() };
        notes.push(format!("uncommitted: {}{more}", files[..files.len().min(5)].join(", ")));
    }
    let stashes = out(repo, &["stash", "list"]).map(|s| s.lines().count()).unwrap_or(0);
    if stashes > 0 {
        notes.push(format!("{stashes} stash entr{}", if stashes == 1 { "y" } else { "ies" }));
    }
    for branch in out(repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads"]).unwrap_or_default().lines() {
        let ahead = out(repo, &["rev-list", "--count", branch, "--not", "--remotes"]).unwrap_or_default();
        if ahead.parse::<u32>().is_ok_and(|n| n > 0) {
            notes.push(format!("branch {branch}: {ahead} commit(s) not on any remote"));
        }
    }
    notes
}

#[cfg(unix)]
fn is_socket(e: &walkdir::DirEntry) -> bool {
    use std::os::unix::fs::FileTypeExt;
    e.file_type().is_socket()
}

#[cfg(not(unix))]
fn is_socket(_: &walkdir::DirEntry) -> bool {
    false
}
