//! Everything git-specific: refusing unsafe sources, and making a copied
//! `.git` a clean, independent repo.

use crate::util::canonical;
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
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
    run_with_input(dir, args, "")
}

fn run_with_input(dir: &Path, args: &[&str], input: &str) -> Result<String> {
    let mut child = git(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git (is it installed?)")?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let input = input.to_string();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let o = child.wait_with_output()?;
    let _ = writer.join();
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

    for e in WalkDir::new(&gd).into_iter().filter_map(Result::ok) {
        let name = e.file_name().to_string_lossy();
        let path = e.path();
        let in_pack_dir = path.parent().is_some_and(|p| p.ends_with("objects/pack"));
        let stale = name.ends_with(".lock")
            || name == "gc.pid"
            || name == "gc.log"
            || name == "fsmonitor--daemon.ipc"
            || (in_pack_dir && name.starts_with("tmp_"))
            || is_socket(&e);
        if stale && !e.file_type().is_dir() {
            let _ = fs::remove_file(path);
        } else if name == "alternates" && path.parent().is_some_and(|p| p.ends_with("objects/info")) {
            // Relative alternates resolve against the objects dir, which moved.
            let rel = path.strip_prefix(&gd)?;
            let src_objects = src_repo.join(".git").join(rel).parent().and_then(Path::parent).map(Path::to_path_buf);
            if let (Some(src_objects), Ok(text)) = (src_objects, fs::read_to_string(path)) {
                let fixed: Vec<String> = text
                    .lines()
                    .map(|l| {
                        let t = l.trim();
                        if t.is_empty() || t.starts_with('#') || Path::new(t).is_absolute() {
                            l.to_string()
                        } else {
                            canonical(&src_objects.join(t)).display().to_string()
                        }
                    })
                    .collect();
                fs::write(path, fixed.join("\n") + "\n")?;
            }
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

    let current = out(dst_repo, &["branch", "--show-current"]).unwrap_or_default();
    if current != branch {
        if has_ref(dst_repo, &format!("refs/heads/{branch}")) {
            run(dst_repo, &["checkout", "-q", branch])
                .with_context(|| format!("switching the copy to existing branch '{branch}'"))?;
        } else if let Some(remote) = remote_branch(dst_repo, branch)? {
            run(dst_repo, &["checkout", "-q", "-b", branch, "--track", &remote])
                .with_context(|| format!("checking out '{remote}' in the copy"))?;
            notes.push(format!("branch {branch} tracks {remote}"));
        } else {
            run(dst_repo, &["checkout", "-q", "-b", branch])?;
        }
    }
    Ok(notes)
}

fn has_ref(repo: &Path, refname: &str) -> bool {
    git(repo)
        .args(["rev-parse", "--verify", "--quiet", refname])
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `<remote>/<branch>` for a branch that only exists on a remote, like
/// `git switch` finds it. Prefers origin when several remotes have it.
fn remote_branch(repo: &Path, branch: &str) -> Result<Option<String>> {
    let remotes = run(repo, &["remote"])?;
    let found: Vec<&str> = remotes.lines().filter(|r| has_ref(repo, &format!("refs/remotes/{r}/{branch}"))).collect();
    Ok(match found.as_slice() {
        [] => None,
        [one] => Some(format!("{one}/{branch}")),
        many if many.contains(&"origin") => Some(format!("origin/{branch}")),
        many => bail!("branch '{branch}' exists on several remotes ({}); create it locally first", many.join(", ")),
    })
}

/// Tracked files of `repo`, forward-slash paths relative to it. Submodules and
/// nested repos are separate repos: list them separately.
pub fn tracked(repo: &Path) -> Result<Vec<String>> {
    Ok(run(repo, &["ls-files", "-z"])?.split('\0').filter(|s| !s.is_empty()).map(String::from).collect())
}

/// Initialized submodules of `repo`, recursively, relative to it.
pub fn submodules(repo: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in run(repo, &["ls-files", "--stage", "-z"])?.split('\0') {
        // "<mode> <sha> <stage>\t<path>"; mode 160000 is a gitlink.
        let Some((meta, path)) = entry.split_once('\t') else { continue };
        if !meta.starts_with("160000 ") || !repo.join(path).join(".git").exists() {
            continue;
        }
        let rel = PathBuf::from(path);
        out.extend(submodules(&repo.join(&rel))?.into_iter().map(|sub| rel.join(sub)));
        out.push(rel);
    }
    Ok(out)
}

/// Work in the copy `dst` that deleting it would lose: uncommitted changes, and
/// commits reachable neither in the original `src` nor from a remote. Looks at
/// branches, tags, HEAD, stash entries and wherever HEAD has been since the copy
/// was made (`since`, unix seconds), so detached and reset-away commits count.
/// Errs when it can't tell, so the caller never mistakes that for "nothing".
pub fn unsaved_work(dst: &Path, src: &Path, since: u64) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let dirty = run(dst, &["status", "--porcelain"])?.lines().count();
    if dirty > 0 {
        problems.push(format!("{dirty} uncommitted change(s)"));
    }

    // (commit, what it is to the user)
    let mut tips: Vec<(String, String)> = Vec::new();
    let refs = run(
        dst,
        &["for-each-ref", "--format=%(objectname) %(*objectname) %(refname)", "refs/heads", "refs/tags", "refs/stash"],
    )?;
    for line in refs.lines() {
        let mut f = line.split(' ');
        let (Some(obj), Some(peeled), Some(name)) = (f.next(), f.next(), f.next()) else { continue };
        let sha = if peeled.is_empty() { obj } else { peeled };
        if name == "refs/stash" {
            for s in run(dst, &["log", "-g", "--format=%H", "refs/stash"])?.lines() {
                tips.push((s.to_string(), "a stash entry (git stash list)".into()));
            }
        } else if let Some(b) = name.strip_prefix("refs/heads/") {
            tips.push((sha.to_string(), format!("branch '{b}'")));
        } else if let Some(t) = name.strip_prefix("refs/tags/") {
            tips.push((sha.to_string(), format!("tag '{t}'")));
        }
    }
    // HEAD, and where it has been since the copy was made: commits made on a
    // detached HEAD, or dropped by a reset. Those a branch or tag still reaches
    // are already covered above.
    let mut loose: Vec<String> = out(dst, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]).into_iter().collect();
    if git_dir(dst)?.join("logs").join("HEAD").exists() {
        for line in run(dst, &["log", "-g", "--date=unix", "--format=%H %gd", "HEAD"])?.lines() {
            // "<sha> HEAD@{<unix time>}"
            let when = line.split_once("@{").and_then(|(_, t)| t.trim_end_matches('}').parse::<u64>().ok());
            if let (Some((sha, _)), Some(when)) = (line.split_once(' '), when)
                && when >= since
            {
                loose.push(sha.to_string());
            }
        }
    }
    if !loose.is_empty() {
        let unreferenced =
            run_with_input(dst, &["rev-list", "--stdin", "--not", "--branches", "--tags"], &lines(&loose))?;
        let unreferenced: HashSet<&str> = unreferenced.lines().collect();
        for sha in loose.iter().filter(|s| unreferenced.contains(s.as_str())) {
            tips.push((sha.clone(), "commits no branch points to (see git reflog)".into()));
        }
    }

    let unsaved = not_in_source(src, &tips.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>())?;
    let mut labels: Vec<&str> = Vec::new();
    for (sha, what) in &tips {
        if unsaved.contains(sha) && !labels.contains(&what.as_str()) && !on_a_remote(dst, sha)? {
            labels.push(what);
        }
    }
    problems.extend(labels.into_iter().map(|w| format!("{w} has commits that exist only in this workbench")));
    Ok(problems)
}

/// The commits among `shas` that `src` doesn't have, or has but can't reach
/// from any ref or reflog (so the next gc would delete them).
fn not_in_source(src: &Path, shas: &[&str]) -> Result<HashSet<String>> {
    if shas.is_empty() {
        return Ok(HashSet::new());
    }
    let mut missing = HashSet::new();
    let mut present = Vec::new();
    let checked = run_with_input(src, &["cat-file", "--batch-check=%(objectname) %(objecttype)"], &lines(shas))?;
    for (sha, line) in shas.iter().zip(checked.lines()) {
        if line.ends_with(" commit") {
            present.push(*sha);
        } else {
            missing.insert(sha.to_string());
        }
    }
    if !present.is_empty() {
        let unreachable =
            run_with_input(src, &["rev-list", "--stdin", "--not", "--all", "--reflog"], &lines(&present))?;
        let unreachable: HashSet<&str> = unreachable.lines().collect();
        missing.extend(present.into_iter().filter(|s| unreachable.contains(s)).map(String::from));
    }
    Ok(missing)
}

/// Revisions for `--stdin`, one per line. A `--not` on the command line doesn't
/// apply to them, so they stay the positive side.
fn lines<S: AsRef<str>>(items: &[S]) -> String {
    items.iter().map(|s| format!("{}\n", s.as_ref())).collect()
}

fn on_a_remote(repo: &Path, sha: &str) -> Result<bool> {
    Ok(!run(repo, &["for-each-ref", "--count=1", "--contains", sha, "refs/remotes"])?.is_empty())
}

fn git_dir(repo: &Path) -> Result<PathBuf> {
    Ok(repo.join(run(repo, &["rev-parse", "--git-dir"])?))
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
