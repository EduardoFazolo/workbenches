//! `wb check`: what deleting a workbench's folder would lose or leave running.
//! It only reports; deleting stays the caller's decision.

use crate::gitfix::out;
use crate::registry::{self, Bench, PORT_BLOCK};
use crate::util::{canonical, tilde};
use std::collections::HashSet;
use std::path::Path;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// Prints the report. Returns whether anything would be lost or left running.
pub fn report(b: &Bench) -> bool {
    let dir = canonical(&b.path);
    let mut clear = true;
    let mut line = |label: &str, text: String, ok: bool| {
        clear &= ok;
        println!("{label:<13} {text}");
    };

    match out(&dir, &["status", "--porcelain"]) {
        None => line("uncommitted", "git couldn't read the repo".into(), false),
        Some(s) if s.is_empty() => line("uncommitted", "none".into(), true),
        Some(s) => {
            let files: Vec<&str> = s.lines().map(|l| l.get(3..).unwrap_or(l)).collect();
            let more = files.len().saturating_sub(10);
            let more = if more > 0 { format!(" (+{more} more)") } else { String::new() };
            line("uncommitted", format!("{}{more}", files[..files.len().min(10)].join(", ")), false);
        }
    }

    let branch = out(&dir, &["branch", "--show-current"]).filter(|s| !s.is_empty()).unwrap_or_else(|| "HEAD".into());
    // Commits only this branch has: not on a remote, not on another local branch
    // (the copy has all the original's branches, so their history drops out).
    let mut args = vec!["rev-list", "--count", "HEAD", "--not", "--remotes"];
    let exclude = format!("--exclude={branch}");
    if branch != "HEAD" {
        args.push(&exclude);
    }
    args.push("--branches");
    let ahead = out(&dir, &args).and_then(|n| n.parse::<u32>().ok());
    // Landed but not pushed: the original has the commits, so nothing is lost.
    let landed = || {
        let head = out(&dir, &["rev-parse", "HEAD"]).unwrap_or_default();
        out(&b.source, &["cat-file", "-e", &format!("{head}^{{commit}}")]).is_some()
    };
    match ahead {
        None => line("unpushed", "git couldn't tell".into(), false),
        Some(0) => line("unpushed", "none".into(), true),
        Some(n) if landed() => {
            line("unpushed", format!("{n} commit(s) on {branch}, not pushed but landed in the original"), true)
        }
        Some(n) => {
            line("unpushed", format!("{n} commit(s) only on {branch}: no remote or other branch has them"), false)
        }
    }

    // The copy starts with the original's stashes; only the others are new.
    let stashes = |repo: &Path| -> HashSet<String> {
        out(repo, &["log", "-g", "--format=%H", "refs/stash"]).unwrap_or_default().lines().map(String::from).collect()
    };
    let (mine, theirs) = (stashes(&dir), stashes(&b.source));
    let new = mine.difference(&theirs).count();
    let copied = mine.len() - new;
    let copied = if copied > 0 { format!(" ({copied} copied from the original)") } else { String::new() };
    line("stashes", if new == 0 { format!("none new{copied}") } else { format!("{new} new{copied}") }, new == 0);

    let procs = running_from(&dir);
    if procs.is_empty() {
        line("processes", "none".into(), true);
    } else {
        line("processes", format!("{} running from it:", procs.len()), false);
        for (pid, cmd) in &procs {
            println!("{:<13}   {pid}  {cmd}", "");
        }
    }

    let busy: Vec<String> =
        (b.port..b.port + PORT_BLOCK).filter(|&p| registry::port_listening(p)).map(|p| p.to_string()).collect();
    let ports = format!("{}-{}", b.port, b.port + PORT_BLOCK - 1);
    if busy.is_empty() {
        line("ports", format!("{ports} free"), true);
    } else {
        line("ports", format!("{} in use (block {ports})", busy.join(", ")), false);
    }

    if clear {
        println!("→ nothing would be lost and nothing runs from it: rm -rf \"{}\"", tilde(&b.path));
    } else {
        println!("→ deleting it now would lose the above or leave it running");
    }
    clear
}

/// Processes started from the folder (working directory inside it) or with its
/// path in their command line (detached helpers). Not `wb` itself or the
/// shells that ran it.
fn running_from(dir: &Path) -> Vec<(Pid, String)> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cwd(UpdateKind::Always).with_cmd(UpdateKind::Always),
    );
    let mut ours = HashSet::new();
    let mut cur = sysinfo::get_current_pid().ok();
    while let Some(pid) = cur.filter(|p| ours.insert(*p)) {
        cur = sys.process(pid).and_then(|p| p.parent());
    }
    let needle = dir.to_string_lossy().into_owned();
    let mut found: Vec<(Pid, String)> = sys
        .processes()
        .iter()
        .filter(|(pid, _)| !ours.contains(pid))
        .filter(|(_, p)| {
            p.cwd().is_some_and(|c| c.starts_with(dir)) || p.cmd().iter().any(|a| a.to_string_lossy().contains(&needle))
        })
        .map(|(pid, p)| {
            let cmd = p.cmd().iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ");
            (*pid, if cmd.is_empty() { p.name().to_string_lossy().into_owned() } else { cmd })
        })
        .collect();
    found.sort_by_key(|(pid, _)| *pid);
    found
}
