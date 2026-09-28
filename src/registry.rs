//! Where workbenches live and what we remember about them.
//!
//! Layout (nothing is ever written into the source repo):
//!   ~/.workbenches/<project>/<name>/          the copy itself
//!   ~/.workbenches/<project>/.meta/<name>.json
//!   ~/.workbenches/<project>/.meta/source     absolute path of the original

use crate::util::{canonical, fnv32};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const PORT_BLOCK: u16 = 10;
const PORT_START: u16 = 3100;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Bench {
    pub name: String,
    pub project: String,
    /// The original folder this was copied from.
    pub source: PathBuf,
    /// The copy.
    pub path: PathBuf,
    pub branch: String,
    /// First port of this bench's block of `PORT_BLOCK`.
    pub port: u16,
    pub created: u64,
}

impl Bench {
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            ("WB_NAME".into(), self.name.clone()),
            ("WB_PROJECT".into(), self.project.clone()),
            ("WB_PATH".into(), self.path.display().to_string()),
            ("WB_SOURCE".into(), self.source.display().to_string()),
            ("WB_PORT".into(), self.port.to_string()),
            ("WB_PORTS".into(), format!("{}-{}", self.port, self.port + PORT_BLOCK - 1)),
            ("PORT".into(), self.port.to_string()),
            ("COMPOSE_PROJECT_NAME".into(), self.compose_project()),
        ]
    }

    /// Compose only allows `[a-z0-9_-]`, starting with a letter or digit. When
    /// that loses information (`fix.a` and `fix-a` would both be `app-fix-a`),
    /// a hash of the real identity keeps the two apart.
    pub fn compose_project(&self) -> String {
        let id = format!("{}-{}", self.project, self.name);
        let clean = sanitize(&id).to_lowercase().trim_start_matches(['-', '_']).to_string();
        if clean == id {
            clean
        } else {
            format!("{clean}-{:08x}", fnv32(format!("{}/{}", self.project, self.name).as_bytes()))
        }
    }
}

pub fn home() -> PathBuf {
    if let Some(p) = std::env::var_os("WB_HOME") {
        return PathBuf::from(p);
    }
    let h = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    h.join(".workbenches")
}

fn meta_path(project: &str, name: &str) -> PathBuf {
    home().join(project).join(".meta").join(format!("{name}.json"))
}

/// Held while choosing a project folder, ports and a name, so parallel
/// `wb new` runs can't pick the same ones. Released on drop.
pub struct Lock(#[allow(dead_code)] File);

pub fn lock() -> Result<Lock> {
    let home = home();
    fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
    let path = home.join(".lock");
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    f.lock().with_context(|| format!("locking {}", path.display()))?;
    Ok(Lock(f))
}

/// Every workbench we know about. Unreadable metadata is reported, not hidden.
pub fn load_all() -> Vec<Bench> {
    let mut out = Vec::new();
    let Ok(projects) = fs::read_dir(home()) else { return out };
    for p in projects.flatten() {
        let Ok(metas) = fs::read_dir(p.path().join(".meta")) else { continue };
        for m in metas.flatten() {
            let path = m.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let parsed =
                fs::read(&path).map_err(anyhow::Error::from).and_then(|d| Ok(serde_json::from_slice::<Bench>(&d)?));
            match parsed {
                Ok(b) => out.push(b),
                Err(e) => eprintln!("wb: skipping unreadable {}: {e:#}", path.display()),
            }
        }
    }
    out.sort_by(|a, b| (&a.project, a.created).cmp(&(&b.project, b.created)));
    out
}

/// `create_new` doubles as a lock: two `wb new x` racing can't both win.
pub fn save(b: &Bench, create_new: bool) -> Result<()> {
    let p = meta_path(&b.project, &b.name);
    fs::create_dir_all(p.parent().unwrap())?;
    let json = serde_json::to_vec_pretty(b)?;
    if create_new {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
            .with_context(|| format!("workbench '{}' already exists", b.name))?;
        f.write_all(&json)?;
    } else {
        fs::write(&p, json)?;
    }
    Ok(())
}

pub fn delete(b: &Bench) -> Result<()> {
    let _lock = lock()?;
    let meta = meta_path(&b.project, &b.name);
    match fs::remove_file(&meta) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", meta.display()));
        }
        _ => {}
    }
    // Drop the project folder once its last bench is gone.
    let proj = home().join(&b.project);
    let any_left = fs::read_dir(proj.join(".meta"))
        .map(|rd| rd.flatten().any(|e| e.path().extension().is_some_and(|x| x == "json")))
        .unwrap_or(false);
    let only_meta = fs::read_dir(&proj).map(|rd| rd.flatten().all(|e| e.file_name() == ".meta")).unwrap_or(false);
    if !any_left && only_meta {
        let _ = remove_dir_all::remove_dir_all(&proj);
    }
    Ok(())
}

pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        bail!("bad name '{name}': use letters, digits, '-', '_' or '.' (not starting with '.' or '-')");
    }
    Ok(())
}

pub fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect()
}

/// Project folder name for a source repo: its basename, or basename-<hash>
/// when another repo with the same basename got there first. Call under `lock()`.
pub fn project_for(source: &Path) -> Result<String> {
    let home = canonical(&home());
    if let Ok(rel) = source.strip_prefix(&home) {
        // Copying a workbench: keep it in the same project.
        if let Some(first) = rel.components().next() {
            return Ok(first.as_os_str().to_string_lossy().into_owned());
        }
    }
    let base = sanitize(&source.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into()));
    let hashed = format!("{base}-{:08x}", fnv32(source.to_string_lossy().as_bytes()));
    for cand in [base, hashed] {
        let marker = home.join(&cand).join(".meta").join("source");
        match fs::read_to_string(&marker) {
            Ok(s) if Path::new(s.trim()) == source => return Ok(cand),
            Ok(_) => continue,
            Err(_) if home.join(&cand).exists() => continue,
            Err(_) => {
                fs::create_dir_all(marker.parent().unwrap())?;
                fs::write(&marker, source.to_string_lossy().as_bytes())?;
                return Ok(cand);
            }
        }
    }
    bail!("could not pick a project folder for {}", source.display())
}

/// First port of a free block. `taken` are other benches' blocks, which count
/// as used even while nothing listens on them. Call under `lock()`.
pub fn allocate_port(taken: &[u16]) -> Result<u16> {
    let mut p = PORT_START;
    while p <= u16::MAX - PORT_BLOCK {
        let clash = taken.iter().any(|&t| t < p + PORT_BLOCK && p < t + PORT_BLOCK);
        if !clash && (p..p + PORT_BLOCK).all(port_free) {
            return Ok(p);
        }
        p += PORT_BLOCK;
    }
    bail!("no free block of {PORT_BLOCK} ports found")
}

fn port_free(p: u16) -> bool {
    TcpListener::bind(("127.0.0.1", p)).is_ok() && TcpListener::bind(("0.0.0.0", p)).is_ok()
}

pub fn port_listening(p: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], p)), Duration::from_millis(80)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bench(project: &str, name: &str) -> Bench {
        Bench {
            name: name.into(),
            project: project.into(),
            source: PathBuf::new(),
            path: PathBuf::new(),
            branch: name.into(),
            port: PORT_START,
            created: 0,
        }
    }

    #[test]
    fn names() {
        for ok in ["login-fix", "v1.2", "a_b", "x"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".hidden", "-flag", "a/b", "../x", "sp ace", &"x".repeat(65)] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn compose_names_never_collide() {
        assert_eq!(bench("app", "fix-a").compose_project(), "app-fix-a");
        let dotted = bench("app", "fix.a").compose_project();
        assert_ne!(dotted, "app-fix-a");
        assert!(dotted.starts_with("app-fix-a-"));
        assert_ne!(bench("App", "x").compose_project(), bench("app", "x").compose_project());
        assert!(bench("-app", "x").compose_project().starts_with("app-x-"));
    }

    #[test]
    fn port_blocks_skip_other_benches() {
        let taken = [PORT_START, PORT_START + PORT_BLOCK];
        let p = allocate_port(&taken).unwrap();
        assert_eq!((p - PORT_START) % PORT_BLOCK, 0);
        assert!(taken.iter().all(|&t| p >= t + PORT_BLOCK || p + PORT_BLOCK <= t));
    }
}
