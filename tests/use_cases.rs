#![cfg(unix)]
//! Acceptance tests: one test per use case, written black-box from the README,
//! `wb --help` and `wb --agents`, run against the real binary. Test names are
//! the use cases; sections group them by command.

mod common;

use common::*;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

// ───────────────────────── Making a workbench ─────────────────────────

#[test]
fn new_makes_an_independent_copy_on_its_own_branch() {
    let env = Env::new();
    let repo = env.repo("app");
    let head = env.head(&repo);

    let wb = env.new_workbench(&repo, "feat");

    assert!(wb.starts_with(&env.wb_home), "copy should live under WB_HOME: {}", wb.display());
    assert_eq!(env.branch(&wb), "feat");
    assert_eq!(env.head(&wb), head, "the copy starts at the original's HEAD");
    assert_eq!(env.branch(&repo), "main", "the original's checkout doesn't move");
    assert_eq!(env.git(&repo, &["branch", "--list", "feat"]), "", "no branch is created in the original");

    let commit = env.commit(&wb, "new.txt", "x\n", "work in copy");
    assert_ne!(env.head(&repo), commit);
    assert!(!repo.join("new.txt").exists());
}

#[test]
fn new_brings_uncommitted_untracked_and_ignored_files_along() {
    let env = Env::new();
    let repo = env.repo("app");
    env.commit(&repo, ".gitignore", ".env\nnode_modules/\n", "ignore");
    write(&repo.join("README.md"), "edited, not committed\n");
    write(&repo.join("notes.txt"), "untracked\n");
    write(&repo.join(".env"), "SECRET=1\n");
    write(&repo.join("node_modules/pkg/index.js"), "module.exports = 1\n");

    let wb = env.new_workbench(&repo, "feat");

    assert_eq!(read(&wb.join("README.md")), "edited, not committed\n");
    assert_eq!(read(&wb.join("notes.txt")), "untracked\n");
    assert_eq!(read(&wb.join(".env")), "SECRET=1\n");
    assert_eq!(read(&wb.join("node_modules/pkg/index.js")), "module.exports = 1\n");
    assert_eq!(env.git(&wb, &["status", "--porcelain"]), env.git(&repo, &["status", "--porcelain"]));
}

#[test]
fn new_reports_where_the_copy_is_and_which_branch_and_ports_it_got() {
    let env = Env::new();
    let repo = env.repo("app");

    let out = env.wb(&["new", "feat"]).in_dir(&repo).succeeds();

    let port = env.port_of(&repo, "feat");
    let path = env.path_of(&repo, "feat");
    assert!(out.mentions("feat"), "{out}");
    assert!(out.mentions(&port.to_string()), "output should show the ports\n{out}");
    assert!(out.mentions(&path.display().to_string()), "output should show the path\n{out}");
}

#[test]
fn new_works_from_a_subfolder_of_the_repo() {
    let env = Env::new();
    let repo = env.repo("app");
    env.commit(&repo, "src/main.txt", "code\n", "src");

    let wb = env.new_workbench(&repo.join("src"), "feat");

    assert!(wb.join("README.md").exists(), "the whole repo is copied, not just src/");
    assert!(wb.join("src/main.txt").exists());
    assert_eq!(env.env_of(&repo, "feat")["WB_SOURCE"], repo.display().to_string());
}

#[test]
fn new_from_copies_a_repo_i_am_not_in() {
    let env = Env::new();
    let repo = env.repo("app");
    let elsewhere = env.dir("elsewhere");

    env.wb(&["new", "spike", "--from", repo.to_str().unwrap()]).in_dir(&elsewhere).succeeds();

    let wb = env.path_of(&repo, "spike");
    assert_eq!(env.branch(&wb), "spike");
    assert_eq!(env.head(&wb), env.head(&repo));
}

#[test]
fn new_with_branch_checks_out_an_existing_local_branch() {
    let env = Env::new();
    let repo = env.repo("app");
    env.git(&repo, &["switch", "-qc", "payments"]);
    let tip = env.commit(&repo, "pay.txt", "pay\n", "payments work");
    env.git(&repo, &["switch", "-q", "main"]);

    env.wb(&["new", "review", "--branch", "payments"]).in_dir(&repo).succeeds();

    let wb = env.path_of(&repo, "review");
    assert_eq!(env.branch(&wb), "payments");
    assert_eq!(env.head(&wb), tip, "existing branch is checked out, not recreated from HEAD");
}

#[test]
fn new_with_branch_creates_a_branch_that_does_not_exist_yet() {
    let env = Env::new();
    let repo = env.repo("app");

    env.wb(&["new", "review", "--branch", "feature/new-thing"]).in_dir(&repo).succeeds();

    let wb = env.path_of(&repo, "review");
    assert_eq!(env.branch(&wb), "feature/new-thing");
    assert_eq!(env.head(&wb), env.head(&repo));
}

#[test]
fn new_checks_out_a_remote_only_branch_tracking_it() {
    let env = Env::new();
    let repo = env.repo("app");
    env.remote(&repo, "origin", "remote");
    env.git(&repo, &["switch", "-qc", "remote-feat"]);
    let tip = env.commit(&repo, "r.txt", "r\n", "remote work");
    env.git(&repo, &["push", "-q", "origin", "remote-feat"]);
    env.git(&repo, &["switch", "-q", "main"]);
    env.git(&repo, &["branch", "-qD", "remote-feat"]);

    env.wb(&["new", "r", "--branch", "remote-feat"]).in_dir(&repo).succeeds();

    let wb = env.path_of(&repo, "r");
    assert_eq!(env.branch(&wb), "remote-feat");
    assert_eq!(env.head(&wb), tip);
    assert_eq!(env.git(&wb, &["rev-parse", "--abbrev-ref", "remote-feat@{upstream}"]), "origin/remote-feat");
}

#[test]
fn new_prefers_origin_when_several_remotes_have_the_branch() {
    let env = Env::new();
    let repo = env.repo("app");
    env.remote(&repo, "upstream", "upstream");
    env.remote(&repo, "origin", "origin");
    for (remote, content) in [("upstream", "from upstream\n"), ("origin", "from origin\n")] {
        env.git(&repo, &["switch", "-qc", "shared", "main"]);
        env.commit(&repo, "s.txt", content, remote);
        env.git(&repo, &["push", "-q", remote, "shared"]);
        env.git(&repo, &["switch", "-q", "main"]);
        env.git(&repo, &["branch", "-qD", "shared"]);
    }
    let origin_tip = env.rev(&repo, "origin/shared");

    env.wb(&["new", "s", "--branch", "shared"]).in_dir(&repo).succeeds();

    let wb = env.path_of(&repo, "s");
    assert_eq!(env.head(&wb), origin_tip);
    assert_eq!(env.git(&wb, &["rev-parse", "--abbrev-ref", "shared@{upstream}"]), "origin/shared");
}

#[test]
fn the_same_branch_can_be_checked_out_in_several_workbenches() {
    let env = Env::new();
    let repo = env.repo("app");

    env.wb(&["new", "a", "--branch", "main"]).in_dir(&repo).succeeds();
    env.wb(&["new", "b", "--branch", "main"]).in_dir(&repo).succeeds();

    assert_eq!(env.branch(&env.path_of(&repo, "a")), "main");
    assert_eq!(env.branch(&env.path_of(&repo, "b")), "main");
    assert_eq!(env.branch(&repo), "main");
}

#[test]
fn each_workbench_gets_its_own_block_of_ten_ports() {
    let env = Env::new();
    let repo = env.repo("app");
    env.new_workbench(&repo, "a");
    env.new_workbench(&repo, "b");

    let (a, b) = (env.env_of(&repo, "a"), env.env_of(&repo, "b"));
    for e in [&a, &b] {
        let port: u32 = e["PORT"].parse().unwrap();
        assert!(port >= 3100, "ports start at 3100: {port}");
        assert_eq!(e["WB_PORT"], e["PORT"]);
        assert_eq!(e["WB_PORTS"], format!("{}-{}", port, port + 9));
    }
    let (pa, pb): (u32, u32) = (a["PORT"].parse().unwrap(), b["PORT"].parse().unwrap());
    assert!(pa.abs_diff(pb) >= 10, "blocks overlap: {pa} and {pb}");
}

#[test]
fn parallel_agents_can_create_workbenches_at_the_same_time() {
    let env = Env::new();
    let repo = env.repo("app");
    let names: Vec<String> = (0..6).map(|i| format!("agent-{i}")).collect();

    let outs: Vec<Out> = std::thread::scope(|s| {
        let handles: Vec<_> = names.iter().map(|n| s.spawn(|| env.wb(&["new", n]).in_dir(&repo).run())).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    for out in &outs {
        assert!(out.ok(), "a parallel `wb new` failed\n{out}");
    }
    let mut ports: Vec<u16> = names.iter().map(|n| env.port_of(&repo, n)).collect();
    ports.sort();
    ports.dedup();
    assert_eq!(ports.len(), names.len(), "every workbench gets its own port block");
    let ls = env.wb(&["ls"]).in_dir(&repo).succeeds();
    for n in &names {
        assert_eq!(env.branch(&env.path_of(&repo, n)), *n);
        assert!(ls.stdout.contains(n.as_str()), "{ls}");
    }
}

#[test]
fn new_refuses_a_name_that_is_already_taken() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    write(&wb.join("mine.txt"), "keep me\n");

    let out = env.wb(&["new", "feat"]).in_dir(&repo).fails();

    assert!(out.mentions("exists"), "{out}");
    assert_eq!(read(&wb.join("mine.txt")), "keep me\n", "the existing workbench is untouched");
}

#[test]
fn new_refuses_invalid_names_without_writing_anywhere_else() {
    let env = Env::new();
    let repo = env.repo("app");

    for name in ["has space", "../escape", "a/b", "", "-dash"] {
        let out = env.wb(&["new", "--", name]).in_dir(&repo).run();
        if name.is_empty() || name.starts_with('-') {
            // clap may reject these as usage errors; they just must not crash or create anything
            assert!(!out.ok(), "name {name:?} was accepted\n{out}");
            assert!(!out.all().contains("panicked"), "{out}");
        } else {
            assert!(!out.ok(), "name {name:?} was accepted\n{out}");
            out.assert_no_crash();
        }
    }
    assert!(!env.root.join("escape").exists());
    assert!(!env.wb_home.join("escape").exists());
    assert!(!env.wb_home.join("app/a/b").exists());
}

#[test]
fn new_refuses_an_invalid_branch_name() {
    let env = Env::new();
    let repo = env.repo("app");

    let out = env.wb(&["new", "x", "--branch", "bad..name"]).in_dir(&repo).fails();

    assert!(out.mentions("branch"), "{out}");
    env.wb(&["path", "x"]).in_dir(&repo).fails();
}

#[test]
fn new_refuses_outside_any_repo() {
    let env = Env::new();
    let empty = env.dir("empty");

    env.wb(&["new", "x"]).in_dir(&empty).fails();

    assert!(!env.wb_home.join("empty/x").exists());
}

#[test]
fn new_refuses_from_a_linked_worktree() {
    let env = Env::new();
    let repo = env.repo("app");
    env.git(&repo, &["worktree", "add", "-q", "../app-wt", "-b", "wt"]);

    let out = env.wb(&["new", "x"]).in_dir(&env.root.join("app-wt")).fails();

    assert!(out.mentions("worktree"), "{out}");
}

#[test]
fn new_refuses_from_inside_a_submodule() {
    let env = Env::new();
    let (repo, _) = repo_with_submodule(&env);

    let out = env.wb(&["new", "x"]).in_dir(&repo.join("lib")).fails();

    assert!(out.mentions("submodule"), "{out}");
}

#[test]
fn new_refuses_a_repo_with_a_git_operation_in_progress() {
    let env = Env::new();
    for op in ["merge", "rebase", "cherry-pick", "bisect"] {
        let repo = env.repo(op);
        match op {
            "bisect" => {
                env.git(&repo, &["bisect", "start"]);
            }
            _ => {
                env.git(&repo, &["switch", "-qc", "other"]);
                let theirs = env.commit(&repo, "README.md", "theirs\n", "theirs");
                env.git(&repo, &["switch", "-q", "main"]);
                env.commit(&repo, "README.md", "ours\n", "ours");
                let args: Vec<&str> = match op {
                    "merge" => vec!["merge", "other"],
                    "rebase" => vec!["rebase", "other"],
                    _ => vec!["cherry-pick", theirs.as_str()],
                };
                assert!(!env.try_git(&repo, &args).ok(), "setup: {op} should stop on a conflict");
            }
        }

        let out = env.wb(&["new", "x"]).in_dir(&repo).fails();

        assert!(out.mentions(op), "refusal should name the {op} in progress\n{out}");
    }
}

#[test]
fn new_refuses_while_a_git_command_is_running() {
    let env = Env::new();
    let repo = env.repo("app");
    write(&repo.join(".git/index.lock"), "");

    let out = env.wb(&["new", "x"]).in_dir(&repo).fails();

    assert!(out.mentions_any(&["index.lock", "running"]), "{out}");
    assert!(repo.join(".git/index.lock").exists(), "wb must not delete the original's lock");
}

#[test]
fn new_on_a_repo_without_commits_does_not_crash() {
    let env = Env::new();
    let repo = env.dir("fresh");
    env.git(&repo, &["init", "-q"]);
    write(&repo.join("a.txt"), "a\n");

    let out = env.wb(&["new", "x"]).in_dir(&repo).run();

    out.assert_no_crash();
    if out.ok() {
        assert_eq!(env.branch(&env.path_of(&repo, "x")), "x");
    }
}

// ───────────────────────── Independent .git ─────────────────────────

#[test]
fn the_original_repo_is_never_written_to() {
    let env = Env::new();
    let repo = env.repo("app");
    write(&repo.join("dirty.txt"), "untracked\n");
    let before = snapshot(&repo);

    let wb = env.new_workbench(&repo, "feat");
    env.wb(&["run", "feat", "--", "sh", "-c", "echo hi > made-in-copy.txt"]).in_dir(&repo).succeeds();
    env.wb(&["ls"]).in_dir(&repo).succeeds();
    env.wb(&["env", "feat"]).in_dir(&repo).succeeds();
    fs::remove_file(wb.join("made-in-copy.txt")).unwrap();
    fs::remove_file(wb.join("dirty.txt")).unwrap();
    env.wb(&["rm", "feat"]).in_dir(&repo).succeeds();

    assert_eq!(snapshot(&repo), before, "the original repo changed");
}

#[test]
fn the_copy_does_not_inherit_the_originals_worktrees() {
    let env = Env::new();
    let repo = env.repo("app");
    env.git(&repo, &["worktree", "add", "-q", "../app-wt", "-b", "wt"]);

    let wb = env.new_workbench(&repo, "feat");

    let list = env.git(&wb, &["worktree", "list", "--porcelain"]);
    assert_eq!(list.matches("worktree ").count(), 1, "only the copy itself:\n{list}");
    env.git(&wb, &["switch", "-q", "wt"]);
    assert_eq!(env.git(&repo, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 2);
}

#[test]
fn a_relative_remote_still_works_in_the_copy() {
    let env = Env::new();
    let repo = env.repo("app");
    let bare = env.root.join("remote.git");
    env.git(&env.root, &["init", "-q", "--bare", "remote.git"]);
    env.git(&repo, &["remote", "add", "origin", "../remote.git"]);
    env.git(&repo, &["push", "-q", "-u", "origin", "main"]);

    let wb = env.new_workbench(&repo, "feat");
    let commit = env.commit(&wb, "f.txt", "f\n", "feat");
    let push = env.try_git(&wb, &["push", "-q", "origin", "feat"]);

    assert!(push.ok(), "push from the copy failed\n{push}");
    assert_eq!(env.rev(&bare, "feat"), commit);
}

#[test]
fn a_copy_of_a_shared_clone_owns_its_objects() {
    let env = Env::new();
    let base = env.repo("base");
    env.git(&env.root, &["clone", "-q", "--shared", base.to_str().unwrap(), "app"]);
    let repo = env.root.join("app");
    write(&repo.join(".git/objects/info/alternates"), "../../../base/.git/objects\n");
    env.git(&repo, &["log", "-1"]);

    let wb = env.new_workbench(&repo, "feat");

    let log = env.try_git(&wb, &["log", "-1", "--format=%s"]);
    assert!(log.ok() && log.stdout.trim() == "initial", "the copy can't read its objects\n{log}");
    let fsck = env.try_git(&wb, &["fsck", "--connectivity-only"]);
    assert!(fsck.ok(), "{fsck}");

    // "Independent": the copy keeps working after the repo it borrowed from moves.
    fs::rename(&base, env.root.join("moved-base")).unwrap();
    let log = env.try_git(&wb, &["log", "-1", "--format=%s"]);
    assert!(log.ok(), "the copy still depends on the donor repo\n{log}");
}

#[test]
fn git_identity_from_include_if_is_kept() {
    let env = Env::new();
    let repo_path = env.root.join("app");
    let identity = env.home.join("work-identity");
    write(&identity, "[user]\n\temail = work@corp.example\n\tname = Work Person\n");
    let mut cfg = read(&env.gitconfig);
    cfg.push_str(&format!("[includeIf \"gitdir:{}/\"]\n\tpath = {}\n", repo_path.display(), identity.display()));
    write(&env.gitconfig, &cfg);
    let repo = env.repo("app");
    assert_eq!(env.git(&repo, &["config", "user.email"]), "work@corp.example");

    let wb = env.new_workbench(&repo, "feat");

    assert_eq!(env.git(&wb, &["config", "user.email"]), "work@corp.example");
    assert_eq!(env.git(&wb, &["config", "user.name"]), "Work Person");
}

#[test]
fn stale_git_pid_and_socket_files_are_not_copied() {
    let env = Env::new();
    let repo = env.repo("app");
    write(&repo.join(".git/gc.pid"), "999999 some-host\n");
    let sock = repo.join(".git/fsmonitor--daemon.ipc");
    let made = env
        .command("python3")
        .args(["-c", "import socket,sys; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1])"])
        .arg(&sock)
        .status()
        .unwrap();
    assert!(made.success() && sock.exists());

    let wb = env.new_workbench(&repo, "feat");

    assert!(!wb.join(".git/gc.pid").exists(), "stale gc.pid copied");
    assert!(!wb.join(".git/fsmonitor--daemon.ipc").exists(), "stale socket copied");
    assert!(repo.join(".git/gc.pid").exists() && sock.exists(), "the original's files are left alone");
}

#[test]
fn the_copy_git_status_matches_the_original() {
    let env = Env::new();
    let repo = env.repo("app");
    env.commit(&repo, "a/b/c.txt", "c\n", "more");
    write(&repo.join("a/b/c.txt"), "changed\n");
    write(&repo.join("new.txt"), "n\n");

    let wb = env.new_workbench(&repo, "feat");

    assert_eq!(
        env.git(&wb, &["status", "--porcelain", "--untracked-files=all"]),
        env.git(&repo, &["status", "--porcelain", "--untracked-files=all"])
    );
    assert!(env.try_git(&wb, &["diff", "--quiet", "--", "README.md"]).ok());
}

// ───────────────────────── Leftover paths ─────────────────────────

#[test]
fn venv_scripts_run_the_copys_python_not_the_originals() {
    let env = Env::new();
    let repo = env.repo("app");
    env.commit(&repo, ".gitignore", ".venv/\n", "ignore venv");
    let made = env.command("python3").args(["-m", "venv", "--without-pip", ".venv"]).current_dir(&repo).status();
    assert!(made.unwrap().success());
    // What pip writes for a console script: an absolute shebang into the venv.
    write_executable(
        &repo.join(".venv/bin/where"),
        &format!("#!{}/.venv/bin/python\nimport sys\nprint(sys.prefix)\n", repo.display()),
    );

    let wb = env.new_workbench(&repo, "feat");

    let out = env.command(wb.join(".venv/bin/where")).output().unwrap();
    let prefix = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(Path::new(&prefix).canonicalize().unwrap(), wb.join(".venv").canonicalize().unwrap());
    let activate = read(&wb.join(".venv/bin/activate"));
    assert!(activate.contains(&wb.display().to_string()), "activate should point at the copy");
    assert!(!activate.contains(&repo.display().to_string()), "activate still points at the original");
}

#[test]
fn pnpm_style_shims_point_at_the_copy() {
    let env = Env::new();
    let repo = env.repo("app");
    env.commit(&repo, ".gitignore", "node_modules/\n", "ignore");
    write_executable(
        &repo.join("node_modules/.bin/tool"),
        &format!("#!/bin/sh\nbasedir={0}/node_modules\ntouch \"{0}/node_modules/.ran\"\n", repo.display()),
    );

    let wb = env.new_workbench(&repo, "feat");
    let ran = env.command(wb.join("node_modules/.bin/tool")).status().unwrap();

    assert!(ran.success());
    assert!(wb.join("node_modules/.ran").exists(), "the shim should act on the copy");
    assert!(!repo.join("node_modules/.ran").exists(), "the shim wrote into the original");
}

#[test]
fn absolute_symlinks_into_the_original_point_at_the_copy() {
    let env = Env::new();
    let repo = env.repo("app");
    env.commit(&repo, ".gitignore", "links/\n", "ignore");
    env.commit(&repo, "data/file.txt", "data\n", "data");
    let outside = env.dir("outside").join("x.txt");
    write(&outside, "outside\n");
    fs::create_dir_all(repo.join("links")).unwrap();
    symlink(repo.join("data/file.txt"), repo.join("links/inside")).unwrap();
    symlink(&outside, repo.join("links/outside")).unwrap();

    let wb = env.new_workbench(&repo, "feat");

    assert_eq!(fs::read_link(wb.join("links/inside")).unwrap(), wb.join("data/file.txt"));
    assert_eq!(fs::read_link(wb.join("links/outside")).unwrap(), outside, "links outside the repo are kept");
}

#[test]
fn git_hooks_point_at_the_copy() {
    let env = Env::new();
    let repo = env.repo("app");
    env.commit(&repo, ".gitignore", "hook-ran\n", "ignore");
    write_executable(
        &repo.join(".git/hooks/pre-commit"),
        &format!("#!/bin/sh\ntouch \"{}/hook-ran\"\n", repo.display()),
    );

    let wb = env.new_workbench(&repo, "feat");
    env.commit(&wb, "x.txt", "x\n", "trigger hook");

    assert!(wb.join("hook-ran").exists(), "the hook should act on the copy");
    assert!(!repo.join("hook-ran").exists(), "the copy's hook wrote into the original");
}

#[test]
fn committed_files_mentioning_the_original_are_left_alone_and_reported() {
    let env = Env::new();
    let repo = env.repo("app");
    let content = format!("{{\"root\": \"{}\"}}\n", repo.display());
    env.commit(&repo, ".vscode/settings.json", &content, "settings");

    let out = env.wb(&["new", "feat"]).in_dir(&repo).succeeds();
    let wb = env.path_of(&repo, "feat");

    assert_eq!(read(&wb.join(".vscode/settings.json")), content);
    assert_eq!(env.git(&wb, &["status", "--porcelain"]), "");
    assert!(out.mentions(".vscode/settings.json"), "the committed file should be reported\n{out}");
}

#[test]
fn committed_absolute_symlinks_are_left_alone() {
    let env = Env::new();
    let repo = env.repo("app");
    env.commit(&repo, "data.txt", "d\n", "data");
    symlink(repo.join("data.txt"), repo.join("link")).unwrap();
    env.git(&repo, &["add", "link"]);
    env.git(&repo, &["commit", "-qm", "committed abs symlink"]);

    let wb = env.new_workbench(&repo, "feat");

    assert_eq!(fs::read_link(wb.join("link")).unwrap(), repo.join("data.txt"));
    assert_eq!(env.git(&wb, &["status", "--porcelain"]), "");
}

#[test]
fn a_sibling_folder_whose_name_starts_like_the_repo_is_left_alone() {
    let env = Env::new();
    let repo = env.repo("app");
    env.commit(&repo, ".gitignore", "local.conf\nsibling-link\n", "ignore");
    let sibling = env.dir("app data");
    let conf = format!("data={}\ncode={}/src\n", sibling.display(), repo.display());
    write(&repo.join("local.conf"), &conf);
    symlink(&sibling, repo.join("sibling-link")).unwrap();

    let wb = env.new_workbench(&repo, "feat");

    let expected = format!("data={}\ncode={}/src\n", sibling.display(), wb.display());
    assert_eq!(read(&wb.join("local.conf")), expected, "only the path to the repo itself is rewritten");
    assert_eq!(fs::read_link(wb.join("sibling-link")).unwrap(), sibling);
}

#[test]
fn committed_files_in_submodules_and_nested_repos_are_left_alone() {
    let env = Env::new();
    let app_path = env.root.join("app");
    let line = format!("path={}\n", app_path.display());
    let lib = env.repo("lib");
    env.commit(&lib, "paths.txt", &line, "lib paths");
    let repo = env.repo("app");
    env.git(&repo, &["submodule", "add", "-q", lib.to_str().unwrap(), "lib"]);
    env.git(&repo, &["commit", "-qm", "add submodule"]);
    let nested = env.repo("app/vendor/nested");
    env.commit(&nested, "paths.txt", &line, "nested paths");
    env.commit(&repo, ".gitignore", "vendor/\n", "ignore vendor");

    let wb = env.new_workbench(&repo, "feat");

    assert_eq!(read(&wb.join("lib/paths.txt")), line);
    assert_eq!(read(&wb.join("vendor/nested/paths.txt")), line);
    assert_eq!(env.git(&wb.join("lib"), &["status", "--porcelain"]), "");
    assert_eq!(env.git(&wb.join("vendor/nested"), &["status", "--porcelain"]), "");
}

#[test]
fn binary_files_are_never_touched() {
    let env = Env::new();
    let repo = env.repo("app");
    let mut bytes = vec![0u8, 1, 2, 0xff];
    bytes.extend_from_slice(repo.display().to_string().as_bytes());
    bytes.extend_from_slice(&[0, 0, 7]);
    fs::write(repo.join("cache.bin"), &bytes).unwrap();

    let wb = env.new_workbench(&repo, "feat");

    assert_eq!(fs::read(wb.join("cache.bin")).unwrap(), bytes);
}

#[test]
fn pid_files_of_processes_running_in_the_original_are_removed() {
    let env = Env::new();
    let repo = env.repo("app");
    env.commit(&repo, ".gitignore", "tmp/\n", "ignore tmp");
    let mut server = env.command("sleep").arg("60").current_dir(&repo).spawn().unwrap();
    write(&repo.join("tmp/pids/server.pid"), &format!("{}\n", server.id()));

    let result = std::panic::catch_unwind(|| {
        let wb = env.new_workbench(&repo, "feat");
        assert!(!wb.join("tmp/pids/server.pid").exists(), "the copy's server would think it's running");
        assert!(repo.join("tmp/pids/server.pid").exists(), "the original's pid file is left alone");
    });
    let _ = server.kill();
    let _ = server.wait();
    result.unwrap();
}

// ───────────────────────── .wb/setup ─────────────────────────

const SETUP_RECORDER: &str =
    "#!/bin/sh\n{ pwd; env | grep -E '^(PORT|WB_[A-Z]+|COMPOSE_PROJECT_NAME)=' ; } > setup-ran.txt\n";

#[test]
fn setup_runs_inside_the_new_workbench_with_its_env() {
    let env = Env::new();
    let repo = env.repo("app");
    write_executable(&repo.join(".wb/setup"), SETUP_RECORDER);
    env.commit(&repo, ".gitignore", "setup-ran.txt\n", "setup");

    let wb = env.new_workbench(&repo, "feat");

    let ran = read(&wb.join("setup-ran.txt"));
    let e = env.env_of(&repo, "feat");
    assert_eq!(ran.lines().next().unwrap(), wb.display().to_string(), "setup runs inside the workbench");
    for key in ["PORT", "WB_PORT", "WB_PORTS", "WB_NAME", "WB_PROJECT", "WB_PATH", "WB_SOURCE", "COMPOSE_PROJECT_NAME"]
    {
        assert!(ran.contains(&format!("{key}={}", e[key])), "setup didn't get {key}\n{ran}");
    }
    assert!(!repo.join("setup-ran.txt").exists(), "setup ran in the original");
}

#[test]
fn no_setup_skips_the_setup_hook() {
    let env = Env::new();
    let repo = env.repo("app");
    write_executable(&repo.join(".wb/setup"), SETUP_RECORDER);
    env.commit(&repo, ".gitignore", "setup-ran.txt\n", "setup");

    env.wb(&["new", "feat", "--no-setup"]).in_dir(&repo).succeeds();

    assert!(!env.path_of(&repo, "feat").join("setup-ran.txt").exists());
}

#[test]
fn a_failing_setup_still_leaves_a_usable_workbench_and_says_so() {
    let env = Env::new();
    let repo = env.repo("app");
    write_executable(&repo.join(".wb/setup"), "#!/bin/sh\necho 'createdb: no postgres' >&2\nexit 3\n");
    env.commit(&repo, "x", "", "setup");

    let out = env.wb(&["new", "feat"]).in_dir(&repo).run();

    out.assert_no_crash();
    assert!(out.mentions("setup"), "the failure should be reported\n{out}");
    let wb = env.path_of(&repo, "feat");
    assert_eq!(env.branch(&wb), "feat");
}

// ───────────────────────── Env, run, shell, path ─────────────────────────

#[test]
fn env_prints_every_documented_variable_as_evalable_exports() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");

    let out = env
        .command("sh")
        .args(["-c", "eval \"$(wb env feat)\" && printf '%s|' \"$PORT\" \"$WB_PORT\" \"$WB_PORTS\" \"$COMPOSE_PROJECT_NAME\" \"$WB_NAME\" \"$WB_PROJECT\" \"$WB_PATH\" \"$WB_SOURCE\""])
        .env("PATH", format!("{}:{}", Path::new(WB).parent().unwrap().display(), std::env::var("PATH").unwrap()))
        .current_dir(&repo)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let v: Vec<&str> = text.split('|').collect();

    let port: u32 = v[0].parse().expect("PORT is a number");
    assert_eq!(v[1], v[0]);
    assert_eq!(v[2], format!("{}-{}", port, port + 9));
    assert!(v[3].starts_with("app-feat-"), "compose name is readable: {}", v[3]);
    assert_eq!(v[4], "feat");
    assert_eq!(v[5], "app");
    assert_eq!(v[6], wb.display().to_string());
    assert_eq!(v[7], repo.display().to_string());
}

#[test]
fn run_executes_in_the_workbench_with_its_env() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");

    let out = env
        .wb(&["run", "feat", "--", "sh", "-c", "pwd; echo $PORT $WB_NAME $COMPOSE_PROJECT_NAME"])
        .in_dir(&repo)
        .succeeds();

    let e = env.env_of(&repo, "feat");
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines[0], wb.display().to_string());
    assert_eq!(lines[1], format!("{} feat {}", e["PORT"], e["COMPOSE_PROJECT_NAME"]));
}

#[test]
fn run_passes_arguments_verbatim_without_a_shell() {
    let env = Env::new();
    let repo = env.repo("app");
    env.new_workbench(&repo, "feat");

    let out = env
        .wb(&["run", "feat", "--", "printf", "[%s]\\n", "$PORT", "a b", "*", "it's", "--flag"])
        .in_dir(&repo)
        .succeeds();

    assert_eq!(out.stdout, "[$PORT]\n[a b]\n[*]\n[it's]\n[--flag]\n");
}

#[test]
fn run_returns_the_commands_exit_code() {
    let env = Env::new();
    let repo = env.repo("app");
    env.new_workbench(&repo, "feat");

    let out = env.wb(&["run", "feat", "--", "sh", "-c", "exit 7"]).in_dir(&repo).run();

    assert_eq!(out.code, Some(7), "{out}");
}

#[test]
fn run_of_a_missing_program_fails_cleanly() {
    let env = Env::new();
    let repo = env.repo("app");
    env.new_workbench(&repo, "feat");

    let out = env.wb(&["run", "feat", "--", "no-such-program-xyz"]).in_dir(&repo).fails();

    assert!(out.mentions("no-such-program-xyz"), "{out}");
}

#[test]
fn shell_opens_a_shell_inside_the_workbench_with_its_env() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");

    let out = env
        .wb(&["shell", "feat"])
        .in_dir(&repo)
        .env("SHELL", "/bin/sh")
        .stdin("pwd\necho \"name=$WB_NAME port=$PORT\"\nexit\n")
        .run();

    out.assert_no_crash();
    let port = env.port_of(&repo, "feat");
    assert!(out.stdout.contains(&wb.display().to_string()), "{out}");
    assert!(out.stdout.contains(&format!("name=feat port={port}")), "{out}");
}

#[test]
fn compose_project_name_is_readable_and_always_valid_for_compose() {
    let env = Env::new();
    let app = env.repo("app");
    env.new_workbench(&app, "login-fix");
    assert!(env.env_of(&app, "login-fix")["COMPOSE_PROJECT_NAME"].starts_with("app-login-fix-"));

    // Compose only accepts lowercase letters, digits, '-' and '_', starting with a letter or digit.
    let mixed = env.repo("MyApp");
    env.new_workbench(&mixed, "Login.Fix");
    let name = env.env_of(&mixed, "Login.Fix")["COMPOSE_PROJECT_NAME"].clone();
    let valid = name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    assert!(valid && name.starts_with("myapp-login-fix"), "not a valid, recognizable compose name: {name}");
}

#[test]
fn compose_names_differ_when_project_and_name_split_differently() {
    let env = Env::new();
    let ab = env.repo("a-b");
    let a = env.repo("a");
    env.new_workbench(&ab, "c");
    env.new_workbench(&a, "b-c");

    assert_ne!(env.env_of(&ab, "c")["COMPOSE_PROJECT_NAME"], env.env_of(&a, "b-c")["COMPOSE_PROJECT_NAME"]);
}

#[test]
fn compose_names_that_would_collide_get_a_hash() {
    let env = Env::new();
    let repo = env.repo("app");
    env.new_workbench(&repo, "fix.a");
    env.new_workbench(&repo, "fix-a");

    let dotted = env.env_of(&repo, "fix.a")["COMPOSE_PROJECT_NAME"].clone();
    let dashed = env.env_of(&repo, "fix-a")["COMPOSE_PROJECT_NAME"].clone();

    assert_ne!(dotted, dashed, "compose projects would collide");
    for n in [&dotted, &dashed] {
        let valid = n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        assert!(valid && n.starts_with("app-"), "not a valid compose name: {n}");
    }
}

#[test]
fn path_prints_the_folder_under_wb_home() {
    let env = Env::new();
    let repo = env.repo("app");
    env.wb(&["new", "feat"]).in_dir(&repo).succeeds();

    let out = env.wb(&["path", "feat"]).in_dir(&repo).succeeds();

    assert_eq!(out.stdout.trim(), env.wb_home.join("app/feat").display().to_string());
    assert!(env.wb_home.join("app/feat/README.md").exists());
}

// ───────────────────────── wb ls ─────────────────────────

#[test]
fn ls_shows_branch_port_status_and_changes() {
    let env = Env::new();
    let repo = env.repo("app");
    env.new_workbench(&repo, "tidy");
    let messy = env.new_workbench(&repo, "messy");
    write(&messy.join("README.md"), "changed\n");
    write(&messy.join("new.txt"), "new\n");
    env.git(&messy, &["switch", "-qc", "other-branch"]);

    let out = env.wb(&["ls"]).in_dir(&repo).succeeds();

    let tidy = line_with(&out, "tidy");
    assert!(tidy.contains(&env.port_of(&repo, "tidy").to_string()), "{out}");
    assert!(tidy.contains("idle") && tidy.contains("clean"), "{out}");
    let messy = line_with(&out, "messy");
    assert!(messy.contains("other-branch"), "ls shows the branch checked out now\n{out}");
    assert!(!messy.contains("clean"), "{out}");
    assert!(messy.contains('2'), "two uncommitted files\n{out}");
}

#[test]
fn ls_shows_only_this_repos_workbenches() {
    let env = Env::new();
    let app = env.repo("app");
    let api = env.repo("api");
    env.new_workbench(&app, "app-work");
    env.new_workbench(&api, "api-work");

    let out = env.wb(&["ls"]).in_dir(&app).succeeds();

    assert!(out.stdout.contains("app-work"), "{out}");
    assert!(!out.stdout.contains("api-work"), "{out}");
}

#[test]
fn ls_all_shows_every_repo_as_project_slash_name() {
    let env = Env::new();
    let app = env.repo("app");
    let api = env.repo("api");
    env.new_workbench(&app, "feat");
    env.new_workbench(&api, "feat");

    let out = env.wb(&["ls", "--all"]).in_dir(&app).succeeds();

    assert!(out.stdout.contains("app/feat") && out.stdout.contains("api/feat"), "{out}");
}

#[test]
fn ls_outside_any_repo_shows_everything() {
    let env = Env::new();
    let app = env.repo("app");
    let api = env.repo("api");
    env.new_workbench(&app, "one");
    env.new_workbench(&api, "two");

    let out = env.wb(&["ls"]).in_dir(&env.dir("nowhere")).succeeds();

    assert!(out.stdout.contains("one") && out.stdout.contains("two"), "{out}");
}

#[test]
fn ls_with_no_workbenches_says_so() {
    let env = Env::new();
    let repo = env.repo("app");

    let out = env.wb(&["ls"]).in_dir(&repo).succeeds();

    assert!(out.mentions("wb new"), "an empty list should say how to make one\n{out}");
}

// ───────────────────────── Name resolution ─────────────────────────

#[test]
fn same_name_in_two_repos_resolves_to_the_repo_you_are_in() {
    let env = Env::new();
    let app = env.repo("app");
    let api = env.repo("api");
    env.new_workbench(&app, "feat");
    env.new_workbench(&api, "feat");

    assert_eq!(env.path_of(&app, "feat"), env.wb_home.join("app/feat"));
    assert_eq!(env.path_of(&api, "feat"), env.wb_home.join("api/feat"));
}

#[test]
fn project_slash_name_works_from_anywhere() {
    let env = Env::new();
    let app = env.repo("app");
    let api = env.repo("api");
    env.new_workbench(&app, "feat");
    env.new_workbench(&api, "feat");

    assert_eq!(env.path_of(&env.dir("nowhere"), "api/feat"), env.wb_home.join("api/feat"));
    assert_eq!(env.path_of(&app, "api/feat"), env.wb_home.join("api/feat"));
    let out = env.wb(&["run", "api/feat", "--", "sh", "-c", "echo $WB_PROJECT"]).in_dir(&app).succeeds();
    assert_eq!(out.stdout.trim(), "api");
}

#[test]
fn ambiguous_name_outside_any_repo_is_refused_with_a_hint() {
    let env = Env::new();
    let app = env.repo("app");
    let api = env.repo("api");
    env.new_workbench(&app, "feat");
    env.new_workbench(&api, "feat");

    let out = env.wb(&["path", "feat"]).in_dir(&env.dir("nowhere")).fails();

    assert!(out.mentions_any(&["app/feat", "project/name"]), "should say how to pick one\n{out}");
}

#[test]
fn ambiguous_name_from_an_unrelated_repo_is_refused_with_a_hint() {
    let env = Env::new();
    let app = env.repo("app");
    let api = env.repo("api");
    let web = env.repo("web");
    env.new_workbench(&app, "feat");
    env.new_workbench(&api, "feat");
    env.new_workbench(&web, "other");

    let out = env.wb(&["path", "feat"]).in_dir(&web).fails();

    assert!(out.mentions("app/feat") && out.mentions("api/feat"), "should list both to pick from\n{out}");
}

#[test]
fn unique_name_works_from_outside_any_repo() {
    let env = Env::new();
    let app = env.repo("app");
    env.new_workbench(&app, "feat");

    assert_eq!(env.path_of(&env.dir("nowhere"), "feat"), env.wb_home.join("app/feat"));
}

#[test]
fn names_resolve_from_inside_a_workbench() {
    let env = Env::new();
    let app = env.repo("app");
    let api = env.repo("api");
    let inside = env.new_workbench(&app, "feat");
    env.new_workbench(&api, "feat");
    env.new_workbench(&app, "sibling");

    assert_eq!(env.path_of(&inside, "feat"), inside);
    assert_eq!(env.env_of(&inside, "feat")["WB_PROJECT"], "app");
    assert_eq!(env.path_of(&inside, "sibling"), env.wb_home.join("app/sibling"));
}

#[test]
fn every_command_refuses_an_unknown_workbench_name() {
    let env = Env::new();
    let repo = env.repo("app");
    env.new_workbench(&repo, "real");

    for args in [
        vec!["path", "ghost"],
        vec!["env", "ghost"],
        vec!["run", "ghost", "--", "true"],
        vec!["shell", "ghost"],
        vec!["land", "ghost"],
        vec!["rm", "ghost"],
        vec!["rm", "--force", "ghost"],
    ] {
        let out = env.wb(&args).in_dir(&repo).fails();
        assert!(out.mentions("ghost"), "should name what wasn't found\n{out}");
    }
    assert!(env.path_of(&repo, "real").exists());
}

#[test]
fn an_unknown_project_in_project_slash_name_is_reported_as_such() {
    let env = Env::new();
    let repo = env.repo("app");
    let real = env.new_workbench(&repo, "real");

    let out = env.wb(&["rm", "--force", "nope/real"]).in_dir(&repo).fails();

    assert!(out.mentions("nope"), "should say project 'nope' wasn't found, not that 'real' doesn't exist\n{out}");
    assert!(real.exists(), "app/real must not be touched");
}

// ───────────────────────── wb land ─────────────────────────

#[test]
fn land_brings_the_branch_into_the_original_without_touching_its_checkout() {
    let env = Env::new();
    let repo = env.repo("app");
    let remote = env.remote(&repo, "origin", "remote");
    write(&repo.join("README.md"), "user's own in-progress edit\n");
    let wb = env.new_workbench(&repo, "feat");
    let commit = env.commit(&wb, "feature.txt", "done\n", "feature");
    write(&wb.join("scratch.txt"), "uncommitted\n");

    env.wb(&["land", "feat"]).in_dir(&repo).succeeds();

    assert_eq!(env.rev(&repo, "refs/heads/feat"), commit);
    assert_eq!(env.branch(&repo), "main");
    assert_eq!(read(&repo.join("README.md")), "user's own in-progress edit\n");
    assert!(!repo.join("feature.txt").exists() && !repo.join("scratch.txt").exists());
    assert!(!env.try_git(&remote, &["rev-parse", "--verify", "--quiet", "feat"]).ok(), "landing never pushes");
}

#[test]
fn land_of_a_branch_checked_out_in_the_original_fetches_and_prints_the_merge_command() {
    let env = Env::new();
    let repo = env.repo("app");
    let before = env.head(&repo);
    env.wb(&["new", "w", "--branch", "main"]).in_dir(&repo).succeeds();
    let wb = env.path_of(&repo, "w");
    let commit = env.commit(&wb, "w.txt", "w\n", "work on main");

    let out = env.wb(&["land", "w"]).in_dir(&repo).succeeds();

    assert_eq!(env.head(&repo), before, "the user's checked-out branch must not move");
    assert!(out.mentions("merge FETCH_HEAD"), "should print the merge command\n{out}");
    env.git(&repo, &["merge", "-q", "--ff-only", "FETCH_HEAD"]);
    assert_eq!(env.head(&repo), commit);
}

#[test]
fn land_refuses_when_the_original_branch_has_commits_the_workbench_lacks() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    env.commit(&wb, "a.txt", "a\n", "first");
    env.wb(&["land", "feat"]).in_dir(&repo).succeeds();
    env.git(&repo, &["switch", "-q", "feat"]);
    let theirs = env.commit(&repo, "b.txt", "b\n", "someone else on feat");
    env.git(&repo, &["switch", "-q", "main"]);
    env.commit(&wb, "c.txt", "c\n", "second");

    env.wb(&["land", "feat"]).in_dir(&repo).fails();

    assert_eq!(env.rev(&repo, "refs/heads/feat"), theirs, "the original's commits are kept");
}

#[test]
fn land_works_when_a_submodule_has_unpushed_commits() {
    let env = Env::new();
    let (repo, _lib) = repo_with_submodule(&env);
    let wb = env.new_workbench(&repo, "feat");
    env.commit(&wb.join("lib"), "lib-change.txt", "x\n", "submodule work");
    env.git(&wb, &["add", "lib"]);
    let bump = env.commit(&wb, "notes.txt", "bumped lib\n", "bump submodule");

    env.wb(&["land", "feat"]).in_dir(&repo).succeeds();

    assert_eq!(env.rev(&repo, "refs/heads/feat"), bump, "the superproject's branch is landed");
}

#[test]
fn land_refuses_a_detached_head_cleanly() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    env.git(&wb, &["switch", "-q", "--detach"]);

    env.wb(&["land", "feat"]).in_dir(&repo).fails();
}

#[test]
fn a_broken_nested_repo_is_left_as_copied_and_reported() {
    let env = Env::new();
    let repo = env.repo("app");
    env.commit(&repo, ".gitignore", "vendor/\n", "ignore vendor");
    let mention = format!("root = {}/vendor/lib\n", repo.display());
    write(&repo.join("vendor/lib/.git"), "gitdir: /nonexistent/modules/lib\n");
    write(&repo.join("vendor/lib/config.txt"), &mention);

    let out = env.wb(&["new", "feat"]).in_dir(&repo).succeeds();

    let wb = env.path_of(&repo, "feat");
    assert_eq!(read(&wb.join("vendor/lib/config.txt")), mention, "files git can't vouch for aren't rewritten");
    assert!(out.mentions("vendor/lib"), "the skipped repo is reported\n{out}");
}

#[cfg(target_os = "linux")]
#[test]
fn new_refuses_a_full_copy_on_a_disk_without_copy_on_write_unless_asked() {
    let env = Env::new();
    let repo = env.repo("app");
    let probe =
        env.command("cp").arg("--reflink=always").arg(repo.join("README.md")).arg(env.root.join("probe")).status();
    if probe.is_ok_and(|s| s.success()) {
        eprintln!("skipped: this disk supports copy-on-write");
        return;
    }

    let out = env.wb(&["new", "feat"]).in_dir(&repo).env("WB_COPY", "0").fails();
    assert!(out.mentions("--copy"), "the refusal says how to go ahead\n{out}");
    env.wb(&["path", "feat"]).in_dir(&repo).fails();

    env.wb(&["new", "feat", "--copy"]).in_dir(&repo).env("WB_COPY", "0").succeeds();
    assert_eq!(env.branch(&env.path_of(&repo, "feat")), "feat");
}

// ───────────────────────── wb rm ─────────────────────────

#[test]
fn rm_deletes_a_clean_workbench_and_frees_its_name() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");

    env.wb(&["rm", "feat"]).in_dir(&repo).succeeds();

    assert!(!wb.exists());
    env.wb(&["path", "feat"]).in_dir(&repo).fails();
    assert!(!env.wb(&["ls"]).in_dir(&repo).succeeds().stdout.contains("feat"));
    assert!(env.new_workbench(&repo, "feat").exists(), "the name can be reused");
}

#[test]
fn rm_refuses_when_uncommitted_changes_would_be_lost() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    write(&wb.join("README.md"), "unsaved edit\n");

    let out = env.wb(&["rm", "feat"]).in_dir(&repo).fails();

    assert!(out.mentions_any(&["uncommitted", "README.md"]), "{out}");
    assert_eq!(read(&wb.join("README.md")), "unsaved edit\n");
}

#[test]
fn rm_refuses_when_an_untracked_file_would_be_lost() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    write(&wb.join("brand-new.txt"), "never added\n");

    env.wb(&["rm", "feat"]).in_dir(&repo).fails();

    assert!(wb.join("brand-new.txt").exists());
}

#[test]
fn rm_refuses_when_a_commit_exists_only_on_its_branch() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    env.git(&wb, &["switch", "-qc", "side-work"]);
    env.commit(&wb, "a.txt", "a\n", "unlanded");
    env.git(&wb, &["switch", "-q", "feat"]);

    let out = env.wb(&["rm", "feat"]).in_dir(&repo).fails();

    assert!(out.mentions("side-work"), "should name the branch at risk\n{out}");
    assert!(wb.exists());
}

#[test]
fn rm_refuses_when_a_stash_would_be_lost() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    write(&wb.join("README.md"), "stashed idea\n");
    env.git(&wb, &["stash", "-q"]);
    assert_eq!(env.git(&wb, &["status", "--porcelain"]), "");

    let out = env.wb(&["rm", "feat"]).in_dir(&repo).fails();

    assert!(out.mentions("stash"), "{out}");
    assert!(wb.exists());
}

#[test]
fn rm_refuses_when_a_commit_exists_only_on_a_tag() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    env.git(&wb, &["switch", "-q", "--detach"]);
    env.commit(&wb, "t.txt", "t\n", "tagged only");
    env.git(&wb, &["tag", "v9"]);
    env.git(&wb, &["switch", "-q", "feat"]);

    let out = env.wb(&["rm", "feat"]).in_dir(&repo).fails();

    assert!(out.mentions_any(&["v9", "tag"]), "{out}");
    assert!(wb.exists());
}

#[test]
fn rm_refuses_when_a_detached_head_commit_exists_only_in_the_reflog() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    env.git(&wb, &["switch", "-q", "--detach"]);
    env.commit(&wb, "d.txt", "d\n", "made while detached");
    env.git(&wb, &["switch", "-q", "feat"]);

    env.wb(&["rm", "feat"]).in_dir(&repo).fails();

    assert!(wb.exists());
}

#[test]
fn rm_refuses_when_a_reset_commit_exists_only_in_the_reflog() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    env.commit(&wb, "r.txt", "r\n", "then reset away");
    env.git(&wb, &["reset", "-q", "--hard", "HEAD~1"]);

    env.wb(&["rm", "feat"]).in_dir(&repo).fails();

    assert!(wb.exists());
}

#[test]
fn rm_refuses_when_a_submodule_commit_was_never_pushed() {
    let env = Env::new();
    let (repo, _lib) = repo_with_submodule(&env);
    env.remote(&repo, "origin", "remote");
    let wb = env.new_workbench(&repo, "feat");
    env.commit(&wb.join("lib"), "lib-change.txt", "x\n", "submodule work");
    env.git(&wb, &["add", "lib"]);
    env.git(&wb, &["commit", "-qm", "bump submodule"]);
    // The superproject's commit is safe on its remote; only the submodule commit is at risk.
    env.git(&wb, &["push", "-q", "--recurse-submodules=no", "-u", "origin", "feat"]);
    assert_eq!(env.git(&wb, &["status", "--porcelain"]), "");

    let out = env.wb(&["rm", "feat"]).in_dir(&repo).fails();

    assert!(out.mentions("lib"), "should name the submodule\n{out}");
    assert!(wb.join("lib/lib-change.txt").exists());
}

#[test]
fn rm_refuses_when_landed_commits_were_deleted_from_the_original() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    env.commit(&wb, "a.txt", "a\n", "landed");
    env.wb(&["land", "feat"]).in_dir(&repo).succeeds();
    env.git(&repo, &["branch", "-qD", "feat"]);

    env.wb(&["rm", "feat"]).in_dir(&repo).fails();

    assert!(wb.exists());
}

#[test]
fn rm_refuses_when_git_cannot_answer() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    let mut config = read(&wb.join(".git/config"));
    config.push_str("[this is not valid\n");
    write(&wb.join(".git/config"), &config);
    assert!(!env.try_git(&wb, &["status"]).ok(), "setup: git should fail in the broken copy");

    env.wb(&["rm", "feat"]).in_dir(&repo).fails();

    assert!(wb.exists());
}

#[test]
fn rm_allows_after_landing() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    env.commit(&wb, "a.txt", "a\n", "work");
    env.wb(&["rm", "feat"]).in_dir(&repo).fails();

    env.wb(&["land", "feat"]).in_dir(&repo).succeeds();
    env.wb(&["rm", "feat"]).in_dir(&repo).succeeds();

    assert!(!wb.exists());
}

#[test]
fn rm_refuses_when_the_remote_no_longer_has_the_pushed_commits() {
    let env = Env::new();
    let repo = env.repo("app");
    let remote = env.remote(&repo, "origin", "remote");
    let wb = env.new_workbench(&repo, "feat");
    let sha = env.commit(&wb, "unique.txt", "unique work\n", "unique");
    env.git(&wb, &["push", "-q", "origin", "feat"]);
    // Someone deletes the branch on the remote and it gets garbage-collected.
    env.git(&remote, &["update-ref", "-d", "refs/heads/feat"]);
    env.git(&remote, &["reflog", "expire", "--expire=now", "--all"]);
    env.git(&remote, &["gc", "-q", "--prune=now"]);
    assert!(!env.try_git(&remote, &["cat-file", "-e", &sha]).ok());

    env.wb(&["rm", "feat"]).in_dir(&repo).fails();

    assert!(wb.exists(), "the only copy of the commit was deleted");
}

#[test]
fn rm_refuses_when_a_process_writes_files_while_shutting_down() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    let ready = env.root.join("ready");
    // A process that saves work to a tracked file when asked to stop.
    let script = "import signal, sys, time, pathlib\n\
def stop(*_):\n    pathlib.Path('README.md').write_text('saved on shutdown')\n    sys.exit(0)\n\
signal.signal(signal.SIGTERM, stop)\npathlib.Path(sys.argv[1]).touch()\nwhile True: time.sleep(0.05)\n";
    let mut child =
        env.command("python3").args(["-c", script, ready.to_str().unwrap()]).current_dir(&wb).spawn().unwrap();
    assert!(eventually(5, || ready.exists()));

    let out = env.wb(&["rm", "feat"]).in_dir(&repo).fails();
    let _ = child.kill();
    let _ = child.wait();

    assert!(wb.exists(), "deleted what the process saved\n{out}");
    assert_eq!(read(&wb.join("README.md")), "saved on shutdown");
}

#[test]
fn rm_keeps_the_deleted_copy_for_a_day() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    write(&wb.join("scratch.txt"), "notes\n");

    let out = env.wb(&["rm", "feat", "--force"]).in_dir(&repo).succeeds();

    assert!(!wb.exists());
    assert!(out.mentions("a day"), "rm says where the copy went\n{out}");
    std::thread::sleep(Duration::from_millis(500)); // give the background purge its chance
    let kept: Vec<_> = fs::read_dir(env.wb_home.join(".trash")).unwrap().flatten().collect();
    assert_eq!(kept.len(), 1, "the deleted copy is kept");
    assert_eq!(read(&kept[0].path().join("scratch.txt")), "notes\n");
}

#[test]
fn rm_allows_after_pushing() {
    let env = Env::new();
    let repo = env.repo("app");
    env.remote(&repo, "origin", "remote");
    let wb = env.new_workbench(&repo, "feat");
    env.commit(&wb, "a.txt", "a\n", "work");
    env.git(&wb, &["push", "-q", "-u", "origin", "feat"]);

    env.wb(&["rm", "feat"]).in_dir(&repo).succeeds();

    assert!(!wb.exists());
}

#[test]
fn rm_force_deletes_despite_unsaved_work() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    env.commit(&wb, "a.txt", "a\n", "unlanded");
    write(&wb.join("README.md"), "unsaved\n");

    env.wb(&["rm", "--force", "feat"]).in_dir(&repo).succeeds();

    assert!(!wb.exists());
    env.wb(&["path", "feat"]).in_dir(&repo).fails();
}

#[test]
fn rm_stops_processes_inside_but_leaves_shells_open() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    let mut guard = Cleanup::new(&env, &repo);
    let worker = env.command("python3").args(["-c", "import time; time.sleep(120)"]).current_dir(&wb).spawn().unwrap();
    let shell = env.command("/bin/sh").current_dir(&wb).stdin(Stdio::piped()).spawn().unwrap();
    guard.children.push(worker);
    guard.children.push(shell);
    std::thread::sleep(Duration::from_millis(300));

    env.wb(&["rm", "feat"]).in_dir(&repo).succeeds();

    assert!(eventually(5, || exited(&mut guard.children[0])), "the process inside was not stopped");
    assert!(!exited(&mut guard.children[1]), "the shell cd'd into the workbench was closed");
    assert!(!wb.exists());
}

// ───────────────────────── Servers ─────────────────────────

#[test]
fn two_workbenches_serve_next_to_the_original_on_their_own_ports() {
    let _ports = serialize_ports();
    let env = Env::new();
    let repo = env.repo("app");
    env.commit(&repo, ".gitignore", "id.txt\n", "ignore");
    write(&repo.join("id.txt"), "original");
    let wb_a = env.new_workbench(&repo, "a");
    let wb_b = env.new_workbench(&repo, "b");
    write(&wb_a.join("id.txt"), "copy a");
    write(&wb_b.join("id.txt"), "copy b");
    let mut guard = Cleanup::new(&env, &repo);
    guard.workbenches = vec!["a".into(), "b".into()];

    let main_port = free_port();
    guard.children.push(
        env.command("python3")
            .args(["-c", PY_SERVER])
            .env("PORT", main_port.to_string())
            .current_dir(&repo)
            .spawn()
            .unwrap(),
    );
    for name in ["a", "b"] {
        guard.children.push(env.wb(&["run", name, "--", "python3", "-c", PY_SERVER]).in_dir(&repo).spawn());
    }
    let (pa, pb) = (env.port_of(&repo, "a"), env.port_of(&repo, "b"));
    for p in [main_port, pa, pb] {
        assert!(eventually(15, || listening(p)), "nothing listening on {p}");
    }

    assert_eq!(http_get(main_port, "/id.txt"), "original");
    assert_eq!(http_get(pa, "/id.txt"), "copy a");
    assert_eq!(http_get(pb, "/id.txt"), "copy b");
    let ls = env.wb(&["ls"]).in_dir(&repo).succeeds();
    assert!(ls.stdout.contains(&format!("serving :{pa}")), "{ls}");
    assert!(ls.stdout.contains(&format!("serving :{pb}")), "{ls}");
}

#[test]
fn rm_stops_a_running_dev_server() {
    let _ports = serialize_ports();
    let env = Env::new();
    let repo = env.repo("app");
    env.new_workbench(&repo, "srv");
    let mut guard = Cleanup::new(&env, &repo);
    guard.workbenches = vec!["srv".into()];
    guard.children.push(env.wb(&["run", "srv", "--", "python3", "-c", PY_SERVER]).in_dir(&repo).spawn());
    let port = env.port_of(&repo, "srv");
    assert!(eventually(15, || listening(port)), "server didn't start on {port}");

    env.wb(&["rm", "srv"]).in_dir(&repo).succeeds();

    assert!(eventually(5, || !listening(port)), "server still answering on {port}");
    assert!(eventually(5, || exited(&mut guard.children[0])), "`wb run` should end when its server is stopped");
}

// ───────────────────────── Things disappearing ─────────────────────────

#[test]
fn a_workbench_folder_deleted_by_hand_does_not_break_wb() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    fs::remove_dir_all(&wb).unwrap();

    env.wb(&["ls"]).in_dir(&repo).run().assert_no_crash();
    env.wb(&["ls", "--all"]).in_dir(&repo).run().assert_no_crash();
    env.wb(&["env", "feat"]).in_dir(&repo).run().assert_no_crash();
    env.wb(&["run", "feat", "--", "true"]).in_dir(&repo).run().assert_no_crash();
    env.wb(&["land", "feat"]).in_dir(&repo).run().assert_no_crash();
    env.wb(&["rm", "feat"]).in_dir(&repo).run().assert_no_crash();

    let again = env.new_workbench(&repo, "feat");
    assert_eq!(env.branch(&again), "feat");
}

#[test]
fn a_deleted_original_repo_does_not_break_wb() {
    let env = Env::new();
    let repo = env.repo("app");
    let wb = env.new_workbench(&repo, "feat");
    fs::remove_dir_all(&repo).unwrap();
    let nowhere = env.dir("nowhere");

    env.wb(&["ls"]).in_dir(&nowhere).run().assert_no_crash();
    env.wb(&["path", "feat"]).in_dir(&nowhere).run().assert_no_crash();
    env.wb(&["land", "feat"]).in_dir(&nowhere).fails();
    env.wb(&["rm", "feat"]).in_dir(&nowhere).run().assert_no_crash();
    env.wb(&["rm", "--force", "feat"]).in_dir(&nowhere).run().assert_no_crash();

    assert!(!wb.exists(), "rm --force should still delete the workbench");
}

// ───────────────────────── The CLI ─────────────────────────

#[test]
fn running_wb_with_no_arguments_shows_help() {
    let env = Env::new();

    let out = env.wb(&[]).in_dir(&env.dir("anywhere")).run();

    out.assert_no_crash();
    assert!(out.mentions("usage") && out.mentions("wb new"), "{out}");
}

#[test]
fn agents_prints_the_agent_guide() {
    let env = Env::new();

    let out = env.wb(&["--agents"]).succeeds();

    let guide = read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src/agents.md"));
    assert_eq!(out.stdout.trim(), guide.trim());
    assert!(out.stdout.starts_with("---\nname: workbenches"));
}

#[test]
fn bad_command_lines_get_a_usage_error() {
    let env = Env::new();
    let repo = env.repo("app");
    env.new_workbench(&repo, "feat");

    for args in [vec!["frobnicate"], vec!["new"], vec!["run", "feat"], vec!["rm"], vec!["ls", "--bogus"]] {
        env.wb(&args).in_dir(&repo).run().assert_usage_error();
    }
}

// ───────────────────────── helpers ─────────────────────────

/// `app` with a submodule `lib` (from a sibling repo), committed.
fn repo_with_submodule(env: &Env) -> (std::path::PathBuf, std::path::PathBuf) {
    let lib = env.repo("lib");
    let repo = env.repo("app");
    env.git(&repo, &["submodule", "add", "-q", lib.to_str().unwrap(), "lib"]);
    env.git(&repo, &["commit", "-qm", "add submodule"]);
    (repo, lib)
}

/// The first stdout line of `wb ls` mentioning `needle`.
fn line_with(out: &Out, needle: &str) -> String {
    out.stdout
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line with {needle:?}\n{out}"))
        .to_string()
}

/// Every file under `dir` with its bytes (symlinks as their target), for before/after comparison.
fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(base: &Path, dir: &Path, acc: &mut Vec<(String, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
        entries.sort();
        for p in entries {
            let rel = p.strip_prefix(base).unwrap().display().to_string();
            let meta = fs::symlink_metadata(&p).unwrap();
            if meta.file_type().is_symlink() {
                acc.push((rel, fs::read_link(&p).unwrap().display().to_string().into_bytes()));
            } else if meta.is_dir() {
                acc.push((rel.clone() + "/", vec![]));
                walk(base, &p, acc);
            } else {
                acc.push((rel, fs::read(&p).unwrap()));
            }
        }
    }
    let mut acc = vec![];
    walk(dir, dir, &mut acc);
    acc
}
