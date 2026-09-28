//! Test harness for the `wb` acceptance suite.
//!
//! Every `Env` is a hermetic sandbox: its own temp folder, `HOME`, `WB_HOME`
//! and global git config. Nothing touches the real `~/.workbenches` or the
//! user's git setup.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub const WB: &str = env!("CARGO_BIN_EXE_wb");

static PORTS: Mutex<()> = Mutex::new(());

/// Ports are machine-global: tests that start servers hold this lock.
pub fn serialize_ports() -> MutexGuard<'static, ()> {
    PORTS.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Env {
    _tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub home: PathBuf,
    pub wb_home: PathBuf,
    pub gitconfig: PathBuf,
}

impl Env {
    pub fn new() -> Env {
        let tmp = tempfile::Builder::new().prefix("wb-test-").tempdir().unwrap();
        // Canonical, so paths written into files match what wb sees (/var -> /private/var on macOS).
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let gitconfig = home.join(".gitconfig");
        fs::write(
            &gitconfig,
            // No background maintenance: newer git detaches `gc`/`maintenance` after
            // commits, and it would change repos while a test inspects them.
            "[protocol \"file\"]\n\tallow = always\n[init]\n\tdefaultBranch = main\n[advice]\n\tdetachedHead = false\n\
             [maintenance]\n\tauto = false\n[gc]\n\tauto = 0\n",
        )
        .unwrap();
        Env { root: root.clone(), home, wb_home: root.join("wbhome"), gitconfig, _tmp: tmp }
    }

    /// A folder inside the sandbox (created).
    pub fn dir(&self, rel: &str) -> PathBuf {
        let p = self.root.join(rel);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// A command with the hermetic environment, run from the sandbox root.
    pub fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut c = Command::new(program);
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("TMPDIR", std::env::temp_dir())
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("WB_HOME", &self.wb_home)
            .env("GIT_CONFIG_GLOBAL", &self.gitconfig)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test Author")
            .env("GIT_AUTHOR_EMAIL", "author@example.com")
            .env("GIT_COMMITTER_NAME", "Test Committer")
            .env("GIT_COMMITTER_EMAIL", "committer@example.com")
            .env("SHELL", "/bin/sh")
            .env("LANG", "C")
            .current_dir(&self.root)
            .stdin(Stdio::null());
        for k in ["USER", "LOGNAME"] {
            if let Some(v) = std::env::var_os(k) {
                c.env(k, v);
            }
        }
        if !cfg!(target_os = "macos") {
            c.env("WB_COPY", "1");
        }
        c
    }

    pub fn wb(&self, args: &[&str]) -> Cmd {
        let mut c = self.command(WB);
        c.args(args);
        Cmd { cmd: c, desc: format!("wb {}", args.join(" ")), stdin: None }
    }

    pub fn try_git(&self, dir: &Path, args: &[&str]) -> Out {
        let mut c = self.command("git");
        c.arg("-C").arg(dir).args(args);
        Cmd { cmd: c, desc: format!("git -C {} {}", dir.display(), args.join(" ")), stdin: None }.run()
    }

    /// Runs git, asserts success, returns trimmed stdout.
    pub fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.try_git(dir, args);
        assert!(out.ok(), "git setup step failed\n{out}");
        out.stdout.trim().to_string()
    }

    /// A new repo with one commit on `main`.
    pub fn repo(&self, rel: &str) -> PathBuf {
        let dir = self.dir(rel);
        self.git(&dir, &["init", "-q"]);
        write(&dir.join("README.md"), &format!("# {rel}\n"));
        self.git(&dir, &["add", "-A"]);
        self.git(&dir, &["commit", "-qm", "initial"]);
        dir
    }

    /// A bare repo at `<root>/<name>.git`, added as `remote` of `repo` with `main` pushed.
    pub fn remote(&self, repo: &Path, remote: &str, name: &str) -> PathBuf {
        let bare = self.root.join(format!("{name}.git"));
        self.git(&self.root, &["init", "-q", "--bare", bare.to_str().unwrap()]);
        self.git(repo, &["remote", "add", remote, bare.to_str().unwrap()]);
        self.git(repo, &["push", "-q", "-u", remote, "main"]);
        bare
    }

    /// Writes `file`, commits everything, returns the new HEAD.
    pub fn commit(&self, dir: &Path, file: &str, content: &str, msg: &str) -> String {
        write(&dir.join(file), content);
        self.git(dir, &["add", "-A"]);
        self.git(dir, &["commit", "-qm", msg]);
        self.head(dir)
    }

    pub fn head(&self, dir: &Path) -> String {
        self.git(dir, &["rev-parse", "HEAD"])
    }

    pub fn branch(&self, dir: &Path) -> String {
        self.git(dir, &["branch", "--show-current"])
    }

    pub fn rev(&self, dir: &Path, rev: &str) -> String {
        self.git(dir, &["rev-parse", "--verify", "--quiet", rev])
    }

    /// `wb new <name>` from `from`, then its folder.
    pub fn new_workbench(&self, from: &Path, name: &str) -> PathBuf {
        self.wb(&["new", name]).in_dir(from).succeeds();
        self.path_of(from, name)
    }

    /// `wb path <name>` run from `from`.
    pub fn path_of(&self, from: &Path, name: &str) -> PathBuf {
        PathBuf::from(self.wb(&["path", name]).in_dir(from).succeeds().stdout.trim())
    }

    /// `wb env <name>` parsed into a map.
    pub fn env_of(&self, from: &Path, name: &str) -> HashMap<String, String> {
        parse_exports(&self.wb(&["env", name]).in_dir(from).succeeds().stdout)
    }

    pub fn port_of(&self, from: &Path, name: &str) -> u16 {
        self.env_of(from, name)["PORT"].parse().expect("PORT is a number")
    }
}

pub fn parse_exports(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("export "))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| {
            let v = v.trim();
            let v = v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')).unwrap_or(v);
            let v = v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v);
            (k.to_string(), v.replace("'\\''", "'"))
        })
        .collect()
}

pub struct Cmd {
    cmd: Command,
    desc: String,
    stdin: Option<String>,
}

impl Cmd {
    pub fn in_dir(mut self, dir: &Path) -> Self {
        self.cmd.current_dir(dir);
        self
    }
    pub fn env(mut self, k: &str, v: impl AsRef<std::ffi::OsStr>) -> Self {
        self.cmd.env(k, v);
        self
    }
    pub fn stdin(mut self, input: &str) -> Self {
        self.stdin = Some(input.to_string());
        self
    }

    pub fn run(mut self) -> Out {
        self.cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        if self.stdin.is_some() {
            self.cmd.stdin(Stdio::piped());
        }
        let mut child = self.cmd.spawn().unwrap_or_else(|e| panic!("can't start {}: {e}", self.desc));
        if let Some(input) = &self.stdin {
            child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        }
        let out = child.wait_with_output().unwrap();
        Out {
            desc: self.desc,
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Runs it and asserts exit 0.
    pub fn succeeds(self) -> Out {
        let out = self.run();
        assert!(out.ok(), "expected success\n{out}");
        out
    }

    /// Runs it and asserts a clean refusal: non-zero exit, no panic/signal, `wb:` message.
    pub fn fails(self) -> Out {
        let out = self.run();
        assert!(!out.ok(), "expected a refusal, but it succeeded\n{out}");
        out.assert_no_crash();
        out
    }

    /// Starts it in the background. Stdout is discarded; stderr is kept, so a
    /// server that dies says why in the test output.
    pub fn spawn(mut self) -> Child {
        self.cmd.stdout(Stdio::null()).spawn().unwrap()
    }
}

pub struct Out {
    pub desc: String,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
    /// stdout and stderr together.
    pub fn all(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
    pub fn mentions(&self, needle: &str) -> bool {
        self.all().to_lowercase().contains(&needle.to_lowercase())
    }
    pub fn mentions_any(&self, needles: &[&str]) -> bool {
        needles.iter().any(|n| self.mentions(n))
    }
    /// Not a crash: exited normally (no signal), no panic, and errors use the `wb:` style.
    pub fn assert_no_crash(&self) {
        assert!(self.code.is_some(), "killed by a signal\n{self}");
        assert!(!self.all().contains("panicked"), "panicked\n{self}");
        if !self.ok() {
            assert!(self.stderr.contains("wb:"), "error without the `wb:` style\n{self}");
            assert!(self.stderr.trim().len() > 4, "error without a readable message\n{self}");
        }
    }
    /// A command-line usage error (from argument parsing): non-zero, no panic, shows usage.
    pub fn assert_usage_error(&self) {
        assert!(self.code.is_some_and(|c| c != 0), "expected a non-zero exit\n{self}");
        assert!(!self.all().contains("panicked"), "panicked\n{self}");
        assert!(self.stderr.to_lowercase().contains("usage"), "no usage hint\n{self}");
    }
}

impl fmt::Display for Out {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "$ {}\nexit: {:?}\n--- stdout\n{}\n--- stderr\n{}", self.desc, self.code, self.stdout, self.stderr)
    }
}

pub fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

pub fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("can't read {}: {e}", path.display()))
}

pub fn write_executable(path: &Path, content: &str) {
    write(path, content);
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Waits up to `secs` for `cond`.
pub fn eventually(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        if cond() {
            return true;
        }
        if Instant::now() > end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn listening(port: u16) -> bool {
    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(300)).is_ok()
}

pub fn http_get(port: u16, path: &str) -> String {
    let out = Command::new("curl")
        .args(["-s", "--max-time", "3", &format!("http://127.0.0.1:{port}{path}")])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

pub fn exited(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(Some(_)))
}

/// A tiny HTTP server serving its current folder on $PORT. Plain `TCPServer`, not
/// `HTTPServer`: the latter does a reverse DNS lookup before listening, which can
/// hang for a long time on CI machines.
pub const PY_SERVER: &str = "import os, socketserver as s, http.server as h\n\
class Server(s.ThreadingMixIn, s.TCPServer):\n    allow_reuse_address = True\n\
Server(('127.0.0.1', int(os.environ['PORT'])), h.SimpleHTTPRequestHandler).serve_forever()";

/// Kills background processes and force-removes workbenches when a test ends, even on failure.
pub struct Cleanup<'a> {
    pub env: &'a Env,
    pub from: PathBuf,
    pub workbenches: Vec<String>,
    pub children: Vec<Child>,
}

impl<'a> Cleanup<'a> {
    pub fn new(env: &'a Env, from: &Path) -> Self {
        Cleanup { env, from: from.to_path_buf(), workbenches: vec![], children: vec![] }
    }
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        for name in &self.workbenches {
            let _ = self.env.wb(&["rm", name]).in_dir(&self.from).run();
        }
        for c in &mut self.children {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
