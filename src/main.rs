mod clone;
mod gitfix;
mod procs;
mod registry;
mod relocate;
mod util;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use registry::{Bench, PORT_BLOCK};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;
use util::{canonical, tilde};

const LONG_ABOUT: &str = "\
Instant, independent copies of a git repo, each on its own branch and its own ports.

A workbench is a full copy of your repo folder, .git included. Not a git worktree:
  - any branch can be checked out in any workbench, even the same one in several
  - .env, node_modules, build caches and uncommitted work all come along
  - deleting one is deleting a folder; nothing is ever written into your repo

On APFS (macOS), btrfs/XFS (Linux) and ReFS/Dev Drive (Windows) copies are
copy-on-write: a 10 GB repo copies in seconds and uses almost no disk until files
change. Other filesystems need --copy (or WB_COPY=1) for a real copy.

Each workbench gets a block of 10 ports (3100-3109, 3110-3119, ...). Commands run
through `wb run` / `wb shell` see PORT, WB_PORT, WB_PORTS, COMPOSE_PROJECT_NAME,
WB_NAME, WB_PROJECT, WB_PATH and WB_SOURCE.";

const AFTER_LONG_HELP: &str = "\
Typical flow:
  wb new login-fix                   copy this repo, branch login-fix, ports 3100-3109
  wb run login-fix -- npm run dev    dev server on PORT=3100, next to your main one
  cd \"$(wb path login-fix)\"          work there (or: wb shell login-fix)
  git commit ...                     commit inside the workbench as usual
  wb land login-fix                  bring the branch back into the original repo
  wb rm login-fix                    stop its processes and delete it

Optional per-repo hook: an executable .wb/setup runs inside every new workbench
with the env above (e.g. create a separate database). Windows: .wb/setup.cmd/.bat/.ps1

Environment:
  WB_HOME   where workbenches live (default ~/.workbenches; keep it on the repo's disk)
  WB_COPY   1 = allow full copies on disks without copy-on-write

For AI agents: `wb --agents` prints complete usage rules as a skill file.";

#[derive(Parser)]
#[command(
    name = "wb",
    version,
    about = "Instant, independent copies of a git repo, each on its own branch and ports.",
    long_about = LONG_ABOUT,
    after_help = "Run `wb --help` for the full guide, `wb <command> --help` for details, `wb --agents` for AI agents.",
    after_long_help = AFTER_LONG_HELP,
    args_conflicts_with_subcommands = true,
)]
struct Cli {
    /// Print the complete usage guide for AI agents (a SKILL.md you can install)
    #[arg(long)]
    agents: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Copy this repo into a new workbench, on its own branch and ports
    #[command(
        verbatim_doc_comment,
        after_long_help = "\
Examples:
  wb new login-fix                        new branch login-fix from the current HEAD
  wb new review --branch feature/payments check out an existing branch instead
  wb new spike --from ~/code/other-repo   copy a repo you're not in
  wb new both                             run from a folder holding several repos:
                                          copies them all, each on branch 'both'"
    )]
    ///
    /// Copies the git repo you're in (or --from) to ~/.workbenches/<project>/<name>,
    /// including .env, node_modules, build folders and uncommitted changes, then:
    ///   - makes the copied .git independent (own HEAD, index and worktree list;
    ///     stale locks removed; relative remotes fixed; your git identity kept)
    ///   - checks out the branch: <name> by default, created if it doesn't exist
    ///   - rewrites leftover absolute paths to the original folder in untracked text
    ///     files (Python venvs, pnpm shims, Bundler config, git hooks, symlinks),
    ///     so the copy never runs or writes into the original
    ///   - removes pid/lock files of processes running in the original
    ///   - reserves a block of 10 ports
    ///   - runs .wb/setup if the repo has one
    ///
    /// Refuses when the copy would stay tied to the original: the source is a git
    /// worktree or submodule, or it's mid-merge/rebase/cherry-pick/bisect.
    New {
        /// Workbench name: letters, digits, '-', '_', '.'
        name: String,
        /// Branch to use (default: the workbench name). An existing branch, local or
        /// only on a remote, is checked out.
        #[arg(short, long)]
        branch: Option<String>,
        /// Folder to copy (default: the git repo containing the current directory)
        #[arg(long, value_name = "PATH")]
        from: Option<PathBuf>,
        /// Allow a full byte copy when the disk can't do copy-on-write clones
        #[arg(long, env = "WB_COPY", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false)]
        copy: bool,
        /// Don't run .wb/setup afterwards
        #[arg(long)]
        no_setup: bool,
    },

    /// List workbenches with branch, ports, running state and changes
    #[command(verbatim_doc_comment)]
    ///
    /// Shows the workbenches of the repo you're in (all of them with --all, or when
    /// you're not in a known repo). Columns:
    ///   NAME     workbench name (project/name with --all)
    ///   BRANCH   branch currently checked out in it
    ///   PORT     first port of its block of 10
    ///   STATUS   "serving :3100" if something listens on its ports,
    ///            "N processes" if something runs inside it, else "idle"
    ///   CHANGES  uncommitted files, "clean", or "?" if git couldn't tell
    ///   AGE      time since it was created
    Ls {
        /// Show workbenches of every repo
        #[arg(short, long)]
        all: bool,
    },

    /// Delete a workbench: stop its processes, then remove the folder
    #[command(verbatim_doc_comment)]
    ///
    /// Refuses if the workbench (or a submodule in it) has uncommitted changes, or
    /// commits the original repo can't reach and no remote has: on a branch or
    /// tag, in the stash, or left behind by a detached HEAD or reset. Land or push
    /// them first, or pass --force to throw them away.
    ///
    /// Stops every process running inside the folder (not shells, so terminal
    /// tabs cd'd into it stay open), then
    /// moves the folder to ~/.workbenches/.trash and deletes it in the background,
    /// so it returns instantly.
    Rm {
        /// Workbench name (or project/name, see `wb ls --all`)
        name: String,
        /// Delete even if work would be lost
        #[arg(short, long)]
        force: bool,
    },

    /// Print a workbench's folder path
    #[command(after_help = "Example:  cd \"$(wb path login-fix)\"")]
    Path {
        /// Workbench name (or project/name, see `wb ls --all`)
        name: String,
    },

    /// Run a command inside a workbench, with PORT and the other WB_* vars set
    #[command(
        verbatim_doc_comment,
        after_help = "\
Examples:
  wb run login-fix -- npm run dev
  wb run login-fix -- sh -c 'npx vite --port $PORT --strictPort'   (Vite ignores PORT)
  wb run login-fix -- docker compose up                   (own COMPOSE_PROJECT_NAME)
  wb run login-fix -- git log --oneline -5

The command runs as given, never through a shell. To use $PORT or other WB_*
vars in its arguments, wrap it in sh -c '...' with single quotes: your own shell
would otherwise expand $PORT before wb sees it. (Windows runs it via cmd /C,
which expands %PORT%.)"
    )]
    ///
    /// Runs in the workbench folder and returns the command's exit code.
    /// Env: PORT, WB_PORT, WB_PORTS, COMPOSE_PROJECT_NAME, WB_NAME, WB_PROJECT,
    /// WB_PATH, WB_SOURCE.
    Run {
        /// Workbench name (or project/name, see `wb ls --all`)
        name: String,
        /// The command and its arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true, value_name = "COMMAND")]
        cmd: Vec<String>,
    },

    /// Open your shell inside a workbench, with its env set
    Shell {
        /// Workbench name (or project/name, see `wb ls --all`)
        name: String,
    },

    /// Print a workbench's env vars as shell exports
    #[command(after_help = "Example:  eval \"$(wb env login-fix)\"")]
    Env {
        /// Workbench name (or project/name, see `wb ls --all`)
        name: String,
    },

    /// Bring a workbench's branch back into the original repo
    #[command(verbatim_doc_comment)]
    ///
    /// Fetches the workbench's current branch into the original repo under the same
    /// name. Only commits move; uncommitted changes stay in the workbench.
    ///
    /// If that branch is checked out in the original, git won't move it, so it's
    /// fetched and you get the merge command to run there. If the original's branch
    /// has commits the workbench lacks, it refuses: merge or rebase first.
    ///
    /// Alternatively just `git push` from the workbench: origin came along.
    Land {
        /// Workbench name (or project/name, see `wb ls --all`)
        name: String,
    },

    /// (internal) delete trashed workbenches in the background
    #[command(name = "__purge", hide = true)]
    Purge,
}

const AGENTS_GUIDE: &str = include_str!("agents.md");

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.agents {
        print!("{AGENTS_GUIDE}");
        return ExitCode::SUCCESS;
    }
    let Some(cmd) = cli.cmd else {
        use clap::CommandFactory;
        let _ = Cli::command().print_long_help();
        return ExitCode::SUCCESS;
    };
    let r = match cmd {
        Cmd::New { name, branch, from, copy, no_setup } => cmd_new(&name, branch, from, copy, no_setup),
        Cmd::Ls { all } => cmd_ls(all),
        Cmd::Rm { name, force } => cmd_rm(&name, force),
        Cmd::Path { name } => resolve(&name).map(|b| println!("{}", b.path.display())),
        Cmd::Run { name, cmd } => return cmd_run(&name, &cmd),
        Cmd::Shell { name } => return cmd_shell(&name),
        Cmd::Env { name } => cmd_env(&name),
        Cmd::Land { name } => cmd_land(&name),
        Cmd::Purge => {
            purge_trash();
            Ok(())
        }
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wb: {e:#}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------- new

fn cmd_new(name: &str, branch: Option<String>, from: Option<PathBuf>, allow_copy: bool, no_setup: bool) -> Result<()> {
    registry::validate_name(name)?;
    let branch = branch.unwrap_or_else(|| name.to_string());
    if !gitfix::valid_branch_name(&branch) {
        bail!("'{branch}' isn't a valid git branch name");
    }

    let from = canonical(&match from {
        Some(p) => p,
        None => std::env::current_dir()?,
    });
    let (root, repos) = find_repos(&from)?;

    let home = registry::home();
    fs::create_dir_all(&home)?;
    let home = canonical(&home);
    if home.starts_with(&root) {
        bail!(
            "{} contains the workbenches folder ({}), so it can't be copied into it.\nSet WB_HOME to a folder outside it.",
            root.display(),
            home.display()
        );
    }
    for r in &repos {
        gitfix::preflight(&root.join(r))?;
    }
    mark_home(&home);

    // Project, name and ports are claimed under one lock; the copy itself isn't.
    let bench = {
        let _lock = registry::lock()?;
        let project = registry::project_for(&root)?;
        let path = home.join(&project).join(name);
        if path.exists() {
            bail!("workbench '{name}' already exists at {}", tilde(&path));
        }
        let taken: Vec<u16> = registry::load_all().iter().map(|b| b.port).collect();
        let bench = Bench {
            name: name.to_string(),
            project,
            source: root.clone(),
            path,
            branch,
            repos,
            port: registry::allocate_port(&taken)?,
            created: util::now_secs(),
        };
        registry::save(&bench, true)?;
        bench
    };

    if let Err(e) = build(&bench, allow_copy) {
        let _ = remove_dir_all::remove_dir_all(&bench.path);
        let _ = registry::delete(&bench);
        return Err(e);
    }
    if !no_setup {
        run_setup(&bench);
    }
    println!();
    println!("  cd \"$(wb path {name})\"");
    println!("  wb run {name} -- <your dev command>");
    Ok(())
}

/// The git repo containing `from`, or a plain folder holding several repos.
fn find_repos(from: &Path) -> Result<(PathBuf, Vec<PathBuf>)> {
    if let Some(top) = gitfix::toplevel(from) {
        return Ok((canonical(&top), vec![PathBuf::new()]));
    }
    let mut repos: Vec<PathBuf> = fs::read_dir(from)
        .with_context(|| format!("reading {}", from.display()))?
        .flatten()
        .filter(|e| e.path().join(".git").exists())
        .map(|e| PathBuf::from(e.file_name()))
        .collect();
    repos.sort();
    if repos.is_empty() {
        bail!("{} isn't in a git repo, and has no git repos directly inside it", from.display());
    }
    Ok((from.to_path_buf(), repos))
}

/// `WB_TIMING=1` prints how long each phase took.
fn phase(label: &str, since: &mut Instant) {
    if std::env::var_os("WB_TIMING").is_some() {
        eprintln!("  [{label}: {:.2}s]", since.elapsed().as_secs_f64());
    }
    *since = Instant::now();
}

fn build(b: &Bench, allow_copy: bool) -> Result<()> {
    let t = Instant::now();
    let mut p = Instant::now();
    let mode = clone::clone_tree(&b.source, &b.path, allow_copy)?;
    phase("clone", &mut p);

    let mut notes = Vec::new();
    for rel in &b.repos {
        let (src, dst) = (b.source.join(rel), b.path.join(rel));
        for n in gitfix::fix_clone(&src, &dst, &b.branch)? {
            notes.push(format!("{}{n}", label(rel)));
        }
    }
    phase("git fixups", &mut p);
    let stats = relocate::relocate(&b.source, &b.path)?;
    phase("relocate", &mut p);

    let how = match mode {
        clone::Mode::Clone => "copy-on-write, uses almost no disk",
        clone::Mode::Copy => "full copy",
    };
    println!("✓ {} ready in {:.1}s ({how})", b.name, t.elapsed().as_secs_f64());
    println!("    path    {}", tilde(&b.path));
    println!("    branch  {}", b.branch);
    println!("    ports   {}-{}  (PORT={})", b.port, b.port + PORT_BLOCK - 1, b.port);
    let mut fixed = Vec::new();
    if stats.files + stats.links > 0 {
        fixed.push(format!("{} files/links that pointed at the original", stats.files + stats.links));
    }
    if stats.removed > 0 {
        fixed.push(format!("removed {} stale pid/lock/socket files", stats.removed));
    }
    if !fixed.is_empty() {
        println!("    fixed   {}", fixed.join(", "));
    }
    for n in notes {
        println!("    note    {n}");
    }
    if !stats.tracked_hits.is_empty() {
        let shown: Vec<&str> = stats.tracked_hits.iter().take(5).map(String::as_str).collect();
        let more = stats.tracked_hits.len().saturating_sub(5);
        println!(
            "  ! {} committed file(s) mention the original folder's path and were left as-is: {}{}",
            stats.tracked_hits.len(),
            shown.join(", "),
            if more > 0 { format!(" (+{more} more)") } else { String::new() }
        );
    }
    for repo in &stats.unlisted {
        let shown = if repo.is_empty() { "." } else { repo.as_str() };
        println!("  ! git couldn't list the committed files of {shown}, so nothing in it was rewritten");
    }
    if !stats.failed.is_empty() {
        println!(
            "  ! {} file(s) still point at the original and couldn't be fixed, so running them may touch it:",
            stats.failed.len()
        );
        for f in stats.failed.iter().take(5) {
            println!("      {f}");
        }
        if stats.failed.len() > 5 {
            println!("      (+{} more)", stats.failed.len() - 5);
        }
    }
    Ok(())
}

/// "sub/dir: " for a repo inside the workbench, "" for the workbench itself.
fn label(rel: &Path) -> String {
    if rel.as_os_str().is_empty() { String::new() } else { format!("{}: ", rel.display()) }
}

/// Keep Spotlight and Time Machine from indexing/backing up N copies.
fn mark_home(home: &Path) {
    #[cfg(target_os = "macos")]
    {
        let marker = home.join(".metadata_never_index");
        if !marker.exists() && fs::write(&marker, b"").is_ok() {
            // Takes ~10s; nobody needs to wait for it.
            let _ = Command::new("tmutil")
                .arg("addexclusion")
                .arg(home)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = home;
}

/// Optional per-repo hook: `.wb/setup` (Windows: setup.cmd / .bat / .ps1),
/// run inside the new copy with the bench env. For the few things that can't
/// be generic: a per-copy database, mapping ports to apps in a monorepo...
fn run_setup(b: &Bench) {
    let dir = b.path.join(".wb");
    let mut cmd = if cfg!(windows) {
        let pick = ["setup.cmd", "setup.bat", "setup.ps1"].iter().map(|f| dir.join(f)).find(|p| p.exists());
        match pick {
            Some(p) if p.extension().is_some_and(|e| e == "ps1") => {
                let mut c = Command::new("powershell");
                c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(p);
                c
            }
            Some(p) => {
                let mut c = Command::new("cmd");
                c.arg("/C").arg(p);
                c
            }
            None => return,
        }
    } else {
        let p = dir.join("setup");
        if !p.exists() {
            return;
        }
        if is_executable(&p) {
            Command::new(p)
        } else {
            let mut c = Command::new("sh");
            c.arg(p);
            c
        }
    };
    println!("  running .wb/setup ...");
    let ok = cmd.current_dir(&b.path).envs(b.env()).status().is_ok_and(|s| s.success());
    if !ok {
        eprintln!(
            "  ! .wb/setup failed. The workbench is still there; fix and re-run it with: wb run {} -- sh .wb/setup",
            b.name
        );
    }
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_: &Path) -> bool {
    false
}

// ---------------------------------------------------------------- lookup

/// `name` or `project/name`. Prefers the project you're currently in.
fn resolve(input: &str) -> Result<Bench> {
    let (proj, name) = match input.split_once('/') {
        Some((p, n)) => (Some(p), n),
        None => (None, input),
    };
    let mut found: Vec<Bench> =
        registry::load_all().into_iter().filter(|b| b.name == name && proj.is_none_or(|p| b.project == p)).collect();
    if found.len() > 1
        && let Some(cur) = current_project()
        && found.iter().any(|b| b.project == cur)
    {
        found.retain(|b| b.project == cur);
    }
    match found.len() {
        0 => bail!("no workbench named '{input}' (see: wb ls --all)"),
        1 => Ok(found.remove(0)),
        _ => {
            let ps: Vec<String> = found.iter().map(|b| format!("{}/{}", b.project, b.name)).collect();
            bail!("'{name}' exists in several projects, pick one: {}", ps.join(", "))
        }
    }
}

fn current_project() -> Option<String> {
    let cwd = canonical(&std::env::current_dir().ok()?);
    let home = canonical(&registry::home());
    if let Ok(rel) = cwd.strip_prefix(&home) {
        return rel.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned());
    }
    let top = gitfix::toplevel(&cwd).map(|p| canonical(&p)).unwrap_or(cwd);
    registry::load_all().into_iter().find(|b| top.starts_with(&b.source)).map(|b| b.project)
}

// ---------------------------------------------------------------- ls

fn cmd_ls(all: bool) -> Result<()> {
    let mut benches = registry::load_all();
    let cur = if all { None } else { current_project() };
    if let Some(p) = &cur {
        benches.retain(|b| &b.project == p);
    }
    if benches.is_empty() {
        println!("no workbenches{}. Make one with: wb new <name>", if cur.is_some() { " for this repo" } else { "" });
        return Ok(());
    }
    let sys = procs::snapshot();
    let rows: Vec<[String; 6]> = benches
        .iter()
        .map(|b| {
            let name = if cur.is_some() { b.name.clone() } else { format!("{}/{}", b.project, b.name) };
            let first = b.path.join(&b.repos[0]);
            let branch = if !b.path.exists() {
                "(folder missing)".into()
            } else if b.repos.len() == 1 {
                gitfix::out(&first, &["branch", "--show-current"])
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "(detached)".into())
            } else {
                format!("{} ({} repos)", b.branch, b.repos.len())
            };
            let changed: Option<usize> = b
                .repos
                .iter()
                .map(|r| gitfix::out(&b.path.join(r), &["status", "--porcelain"]).map(|s| s.lines().count()))
                .sum();
            let listening: Vec<u16> = (b.port..b.port + PORT_BLOCK).filter(|&p| registry::port_listening(p)).collect();
            let nprocs = procs::inside(&sys, &canonical(&b.path)).len();
            let status = if !listening.is_empty() {
                let ports: Vec<String> = listening.iter().map(|p| format!(":{p}")).collect();
                format!("serving {}", ports.join(" "))
            } else if nprocs > 0 {
                format!("{nprocs} process{}", if nprocs == 1 { "" } else { "es" })
            } else {
                "idle".into()
            };
            let changes = match changed {
                Some(0) => "clean".into(),
                Some(n) => format!("{n} changed"),
                None => "?".into(),
            };
            [name, branch, format!("{}", b.port), status, changes, util::age(b.created)]
        })
        .collect();

    let header = ["NAME", "BRANCH", "PORT", "STATUS", "CHANGES", "AGE"];
    let mut w = header.map(str::len);
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }
    let line = |cells: [&str; 6]| {
        let s: Vec<String> = cells.iter().enumerate().map(|(i, c)| format!("{c:<width$}", width = w[i])).collect();
        println!("{}", s.join("  ").trim_end());
    };
    line(header);
    for r in &rows {
        line([&r[0], &r[1], &r[2], &r[3], &r[4], &r[5]]);
    }
    Ok(())
}

// ---------------------------------------------------------------- rm

fn cmd_rm(name: &str, force: bool) -> Result<()> {
    let b = resolve(name)?;
    if b.path.exists() && !force {
        let problems = unsaved_work(&b).with_context(|| {
            format!("couldn't check '{}' for unsaved work, so it wasn't deleted (--force deletes anyway)", b.name)
        })?;
        if !problems.is_empty() {
            bail!(
                "'{}' has work that would be lost:\n  - {}\nLand it (wb land {}), push it, or delete anyway with: wb rm {} --force",
                b.name,
                problems.join("\n  - "),
                b.name,
                b.name
            );
        }
    }

    let mut p = Instant::now();
    phase("safety checks", &mut p);
    if b.path.exists() {
        let dir = canonical(&b.path);
        let procs::Stopped { stopped, shells } = procs::stop_inside(&dir);
        if stopped > 0 {
            println!("  stopped {stopped} process{}", if stopped == 1 { "" } else { "es" });
        }
        if shells > 0 {
            println!("  note: {shells} shell(s) had their working directory inside it; cd them elsewhere");
        }
        phase("stop processes", &mut p);
        for rel in &b.repos {
            let dst = b.path.join(rel);
            let _ = gitfix::git(&dst).args(["fsmonitor--daemon", "stop"]).output();
            let _ = gitfix::git(&dst).args(["maintenance", "unregister", "--force"]).output();
        }
        if has_compose_file(&b.path) {
            println!(
                "  note: if you started docker compose here, stop it with: docker compose -p {} down",
                b.compose_project()
            );
        }
        trash(&b)?;
        phase("delete folder", &mut p);
    }
    registry::delete(&b)?;
    println!("✓ removed {}", b.name);
    if std::env::current_dir()
        .is_ok_and(|c| canonical(&c).starts_with(canonical(&registry::home()).join(&b.project).join(&b.name)))
    {
        println!("  (your shell was inside it; cd somewhere else)");
    }
    Ok(())
}

/// Deleting 100k files takes seconds; renaming the folder is instant on every
/// OS. So move it into ~/.workbenches/.trash and let a detached `wb __purge`
/// delete it after we've returned.
fn trash(b: &Bench) -> Result<()> {
    let bin = registry::home().join(".trash");
    fs::create_dir_all(&bin)?;
    let dest = bin.join(format!("{}-{}-{}", b.project, b.name, std::process::id()));
    if fs::rename(&b.path, &dest).is_err() {
        // e.g. Windows with a file still open: fall back to deleting in place.
        return remove_dir_all::remove_dir_all(&b.path).with_context(|| format!("deleting {}", b.path.display()));
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut c = Command::new(exe);
        c.arg("__purge")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            c.process_group(0); // survive the terminal closing
        }
        if c.spawn().is_err() {
            purge_trash();
        }
    }
    Ok(())
}

fn purge_trash() {
    if let Ok(rd) = fs::read_dir(registry::home().join(".trash")) {
        for e in rd.flatten() {
            let _ = remove_dir_all::remove_dir_all(e.path());
        }
    }
}

/// Everything `wb rm` would lose, in every repo of the workbench and their submodules.
fn unsaved_work(b: &Bench) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    for rel in &b.repos {
        let root = b.path.join(rel);
        let mut repos = vec![rel.clone()];
        repos.extend(gitfix::submodules(&root)?.into_iter().map(|s| rel.join(s)));
        for r in repos {
            for p in gitfix::unsaved_work(&b.path.join(&r), &b.source.join(&r), b.created)? {
                problems.push(format!("{}{p}", label(&r)));
            }
        }
    }
    Ok(problems)
}

fn has_compose_file(dir: &Path) -> bool {
    ["compose.yaml", "compose.yml", "docker-compose.yml", "docker-compose.yaml"].iter().any(|f| dir.join(f).exists())
}

// ---------------------------------------------------------------- run / shell / env

/// Become `cmd`. On Unix `wb` is replaced by it, so signals (Ctrl-C, a
/// supervisor's SIGTERM) reach it directly and its exit status is ours.
/// Elsewhere, run it and pass its exit code on.
fn hand_over(mut cmd: Command, what: &str) -> ExitCode {
    #[cfg(unix)]
    let err = {
        use std::os::unix::process::CommandExt;
        cmd.exec()
    };
    #[cfg(not(unix))]
    let err = match cmd.status() {
        Ok(s) => return ExitCode::from(s.code().unwrap_or(1).clamp(0, 255) as u8),
        Err(e) => e,
    };
    eprintln!("wb: couldn't start {what}: {err}");
    ExitCode::FAILURE
}

fn cmd_run(name: &str, cmd: &[String]) -> ExitCode {
    let b = match resolve(name) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("wb: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let mut c = if cfg!(windows) {
        // cmd resolves npm.cmd & friends, and expands %PORT%.
        let mut c = Command::new("cmd");
        c.arg("/C").args(cmd);
        c
    } else {
        let mut c = Command::new(&cmd[0]);
        c.args(&cmd[1..]);
        c
    };
    c.current_dir(&b.path).envs(b.env());
    hand_over(c, &cmd[0])
}

fn cmd_shell(name: &str) -> ExitCode {
    let b = match resolve(name) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("wb: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let shell = if cfg!(windows) {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    };
    println!("entering {} ({}), PORT={}. exit to leave.", b.name, tilde(&b.path), b.port);
    let mut c = Command::new(&shell);
    c.current_dir(&b.path).envs(b.env());
    hand_over(c, &shell)
}

fn cmd_env(name: &str) -> Result<()> {
    let b = resolve(name)?;
    for (k, v) in b.env() {
        if cfg!(windows) {
            println!("$env:{k}='{}'", v.replace('\'', "''"));
        } else {
            println!("export {k}='{}'", v.replace('\'', "'\\''"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- land

fn cmd_land(name: &str) -> Result<()> {
    let b = resolve(name)?;
    for rel in &b.repos {
        let (dst, src) = (b.path.join(rel), b.source.join(rel));
        let label = label(rel);
        let Some(branch) = gitfix::out(&dst, &["branch", "--show-current"]).filter(|s| !s.is_empty()) else {
            bail!("{label}the workbench isn't on a branch (detached HEAD); check one out first");
        };
        let dirty = gitfix::run(&dst, &["status", "--porcelain"])?.lines().count();
        if dirty > 0 {
            println!("  ! {label}{dirty} uncommitted change(s) stay behind; only commits are landed");
        }
        let dst_s = dst.display().to_string();
        if checked_out_in(&src, &branch) {
            // Git won't move a checked-out branch under someone's feet.
            gitfix::run(
                &src,
                &["fetch", "--no-tags", "--recurse-submodules=no", &dst_s, &format!("refs/heads/{branch}")],
            )?;
            println!("✓ {label}fetched '{branch}'. It's checked out in the original, so merge it there:");
            println!("    git -C \"{}\" merge FETCH_HEAD", src.display());
        } else {
            let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
            if let Err(e) = gitfix::run(&src, &["fetch", "--no-tags", "--recurse-submodules=no", &dst_s, &refspec]) {
                if format!("{e:#}").contains("non-fast-forward") {
                    bail!("{label}'{branch}' in the original has commits the workbench doesn't; merge or rebase first");
                }
                return Err(e.context(format!("{label}landing '{branch}'")));
            }
            println!("✓ {label}branch '{branch}' is now in {}", tilde(&src));
        }
    }
    Ok(())
}

fn checked_out_in(repo: &Path, branch: &str) -> bool {
    gitfix::out(repo, &["worktree", "list", "--porcelain"])
        .is_some_and(|s| s.lines().any(|l| l == format!("branch refs/heads/{branch}")))
}
