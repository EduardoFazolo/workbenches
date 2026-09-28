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
use util::{canonical, tilde};

const NAME_HELP: &str = "Workbench name (or project/name, see `wb ls --all`)";

#[derive(Parser)]
#[command(
    name = "wb",
    version,
    about = "Independent copies of a git repo, each on its own branch and block of ports.",
    after_help = "\
Typical flow:
  wb new login-fix                   copy this repo; branch login-fix; ports 3100-3109
  wb run login-fix -- npm run dev    run it with PORT=3100
  wb land login-fix                  bring its commits back into this repo
  wb rm login-fix                    stop it and delete it

Environment: WB_HOME (where copies live, default ~/.workbenches), WB_COPY=1 (same as --copy).
Details: https://github.com/EduardoFazolo/workbenches. `wb --agents` prints a guide for AI coding agents.",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Print the usage guide for AI coding agents (a SKILL.md)
    #[arg(long)]
    agents: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Copy the repo you're in to a new workbench on its own branch and ports
    #[command(after_help = "\
Examples:
  wb new login-fix                          new branch login-fix from HEAD
  wb new review --branch feature/payments   check out an existing branch
  wb new spike --from ~/code/other-repo     copy a repo you're not in")]
    New {
        /// Letters, digits, '-', '_' and '.'
        name: String,
        /// Branch to check out, local or remote-only (default: the name, created from HEAD)
        #[arg(short, long)]
        branch: Option<String>,
        /// Repo to copy (default: the one you're in)
        #[arg(long, value_name = "PATH")]
        from: Option<PathBuf>,
        /// Allow a full copy on a disk without copy-on-write
        #[arg(long, env = "WB_COPY", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false)]
        copy: bool,
        /// Don't run .wb/setup
        #[arg(long)]
        no_setup: bool,
    },

    /// List this repo's workbenches: branch, port, what's running, changes
    Ls {
        /// Every repo's workbenches
        #[arg(short, long)]
        all: bool,
    },

    /// Stop what runs inside a workbench and delete it; refuses if work would be lost
    Rm {
        #[arg(help = NAME_HELP)]
        name: String,
        /// Delete even if work would be lost
        #[arg(short, long)]
        force: bool,
    },

    /// Print a workbench's folder
    #[command(after_help = "Example:  cd \"$(wb path login-fix)\"")]
    Path {
        #[arg(help = NAME_HELP)]
        name: String,
    },

    /// Run a command in a workbench with PORT and the WB_* variables set
    #[command(after_help = "\
Examples:
  wb run login-fix -- npm run dev
  wb run login-fix -- sh -c 'vite --port $PORT --strictPort'

The command runs as given, not through a shell. Use sh -c '...' (single quotes)
to put $PORT in its arguments.")]
    Run {
        #[arg(help = NAME_HELP)]
        name: String,
        /// The command and its arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true, value_name = "COMMAND")]
        cmd: Vec<String>,
    },

    /// Open your shell in a workbench with its variables set
    Shell {
        #[arg(help = NAME_HELP)]
        name: String,
    },

    /// Print a workbench's variables as shell exports
    #[command(after_help = "Example:  eval \"$(wb env login-fix)\"")]
    Env {
        #[arg(help = NAME_HELP)]
        name: String,
    },

    /// Fetch a workbench's current branch into the original repo
    Land {
        #[arg(help = NAME_HELP)]
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
        Cmd::Path { name } => resolve_ready(&name).map(|b| println!("{}", b.path.display())),
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
    let root = gitfix::toplevel(&from)
        .map(|p| canonical(&p))
        .with_context(|| format!("{} isn't inside a git repo", from.display()))?;

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
    gitfix::preflight(&root)?;
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
            port: registry::allocate_port(&taken)?,
            created: util::now_secs(),
            creating: true,
        };
        registry::save(&bench, true)?;
        bench
    };

    if let Err(e) = build(&bench, allow_copy) {
        let _ = remove_dir_all::remove_dir_all(&bench.path);
        let _ = registry::delete(&bench);
        return Err(e);
    }
    let bench = Bench { creating: false, ..bench };
    registry::save(&bench, false)?;
    if !no_setup {
        run_setup(&bench);
    }
    println!();
    println!("  cd \"$(wb path {name})\"");
    println!("  wb run {name} -- <your dev command>");
    Ok(())
}

fn build(b: &Bench, allow_copy: bool) -> Result<()> {
    let started = std::time::Instant::now();
    let mode = clone::clone_tree(&b.source, &b.path, allow_copy)?;
    let notes = gitfix::fix_clone(&b.source, &b.path, &b.branch)?;
    let stats = relocate::relocate(&b.source, &b.path)?;

    let how = match mode {
        clone::Mode::Clone => "copy-on-write, uses almost no disk",
        clone::Mode::Copy => "full copy",
    };
    println!("✓ {} ready in {:.1}s ({how})", b.name, started.elapsed().as_secs_f64());
    println!("    path    {}", tilde(&b.path));
    println!("    branch  {}", b.branch);
    println!("    ports   {}-{}  (PORT={})", b.port, b.port + PORT_BLOCK - 1, b.port);
    if stats.fixed > 0 {
        println!("    fixed   {} venv/shim/hook file(s) that pointed at the original", stats.fixed);
    }
    if stats.removed > 0 {
        println!("    removed {} pid/lock file(s) of servers running in the original", stats.removed);
    }
    for n in notes {
        println!("    note    {n}");
    }
    for f in &stats.failed {
        println!("  ! couldn't fix {f}");
    }
    Ok(())
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

/// `resolve`, for every command but `rm`: refuses a workbench still being created.
fn resolve_ready(input: &str) -> Result<Bench> {
    let b = resolve(input)?;
    if b.creating {
        bail!(
            "'{}' is still being created. If `wb new` was interrupted, remove it with: wb rm {} --force",
            b.name,
            b.name
        );
    }
    Ok(b)
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
            let branch = if !b.path.exists() {
                "(folder missing)".into()
            } else {
                gitfix::out(&b.path, &["branch", "--show-current"])
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "(detached)".into())
            };
            let changed = gitfix::out(&b.path, &["status", "--porcelain"]).map(|s| s.lines().count());
            let listening: Vec<u16> = (b.port..b.port + PORT_BLOCK).filter(|&p| registry::port_listening(p)).collect();
            let nprocs = procs::inside(&sys, &canonical(&b.path)).len();
            let status = if b.creating {
                "creating".into()
            } else if !listening.is_empty() {
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
    if b.path.exists() {
        if !force {
            refuse_if_unsaved(&b)?;
        }
        let dir = canonical(&b.path);
        let procs::Stopped { stopped, shells } = procs::stop_inside(&dir);
        if stopped > 0 {
            println!("  stopped {stopped} process{}", if stopped == 1 { "" } else { "es" });
        }
        if shells > 0 {
            println!("  note: {shells} shell(s) had their working directory inside it; cd them elsewhere");
        }
        // Best effort: background git helpers that would outlive the folder.
        let _ = gitfix::git(&b.path).args(["fsmonitor--daemon", "stop"]).output();
        let _ = gitfix::git(&b.path).args(["maintenance", "unregister", "--force"]).output();
        if has_compose_file(&b.path) {
            println!(
                "  note: if you started docker compose here, stop it with: docker compose -p {} down",
                b.compose_project()
            );
        }
        trash(&b)?;
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

/// How long deleted workbenches stay in the trash. The checks before deleting
/// are deliberately simple; this is the real safety net. Three days covers a
/// weekend.
const TRASH_KEEP_SECS: u64 = 3 * 24 * 60 * 60;

/// Deleting 100k files takes seconds; renaming the folder is instant. So move it
/// into ~/.workbenches/.trash and let a detached `wb __purge` delete whatever
/// has been there longer than `TRASH_KEEP_SECS`.
fn trash(b: &Bench) -> Result<()> {
    let bin = registry::home().join(".trash");
    fs::create_dir_all(&bin)?;
    let dest = bin.join(format!("{}-{}-{}-{}", b.project, b.name, std::process::id(), util::now_secs()));
    if fs::rename(&b.path, &dest).is_err() {
        // e.g. Windows with a file still open: fall back to deleting in place.
        return remove_dir_all::remove_dir_all(&b.path).with_context(|| format!("deleting {}", b.path.display()));
    }
    println!("  kept in {} for 3 days, in case you need it back", tilde(&dest));
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
        let _ = c.spawn();
    }
    Ok(())
}

/// Deletes trashed workbenches older than `TRASH_KEEP_SECS`. Each folder name
/// ends in the unix time it was trashed.
fn purge_trash() {
    let Ok(rd) = fs::read_dir(registry::home().join(".trash")) else { return };
    let now = util::now_secs();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let trashed = name.rsplit('-').next().and_then(|t| t.parse::<u64>().ok()).unwrap_or(0);
        if now.saturating_sub(trashed) >= TRASH_KEEP_SECS {
            let _ = remove_dir_all::remove_dir_all(e.path());
        }
    }
}

fn refuse_if_unsaved(b: &Bench) -> Result<()> {
    let problems = gitfix::unsaved_work(&b.path, &b.source).with_context(|| {
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
    Ok(())
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
    let b = match resolve_ready(name) {
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
    let b = match resolve_ready(name) {
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
    let b = resolve_ready(name)?;
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
    let b = resolve_ready(name)?;
    let (dst, src) = (&b.path, &b.source);
    let Some(branch) = gitfix::out(dst, &["branch", "--show-current"]).filter(|s| !s.is_empty()) else {
        bail!("the workbench isn't on a branch (detached HEAD); check one out first");
    };
    let dirty = gitfix::run(dst, &["status", "--porcelain"])?.lines().count();
    if dirty > 0 {
        println!("  ! {dirty} uncommitted change(s) stay behind; only commits are landed");
    }
    let dst_s = dst.display().to_string();
    // Submodule commits aren't landed (the README says so); don't let git try.
    let fetch = ["fetch", "--no-tags", "--recurse-submodules=no", &dst_s];
    if checked_out_in(src, &branch) {
        // Git won't move a checked-out branch under someone's feet.
        gitfix::run(src, &[&fetch[..], &[&format!("refs/heads/{branch}")]].concat())?;
        println!("✓ fetched '{branch}'. It's checked out in the original, so merge it there:");
        println!("    git -C \"{}\" merge FETCH_HEAD", src.display());
    } else {
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        if let Err(e) = gitfix::run(src, &[&fetch[..], &[&refspec]].concat()) {
            if format!("{e:#}").contains("non-fast-forward") {
                bail!("'{branch}' in the original has commits the workbench doesn't; merge or rebase first");
            }
            return Err(e.context(format!("landing '{branch}'")));
        }
        println!("✓ branch '{branch}' is now in {}", tilde(src));
    }
    Ok(())
}

fn checked_out_in(repo: &Path, branch: &str) -> bool {
    gitfix::out(repo, &["worktree", "list", "--porcelain"])
        .is_some_and(|s| s.lines().any(|l| l == format!("branch refs/heads/{branch}")))
}
