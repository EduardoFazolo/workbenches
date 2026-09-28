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
    /// Name and ports are claimed, but `wb new` hasn't finished the copy.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub creating: bool,
}

/// A creation this old was interrupted, not slow.
const ABANDONED_AFTER_SECS: u64 = 60 * 60;

impl Bench {
    /// Deleting a workbench's folder is how you remove it, so a record only
    /// counts while its folder exists, or while its creation is under way.
    pub fn is_live(&self) -> bool {
        if self.creating {
            crate::util::now_secs().saturating_sub(self.created) < ABANDONED_AFTER_SECS
        } else {
            self.path.exists()
        }
    }

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

    /// Compose only allows `[a-z0-9_-]`, starting with a letter or digit, and
    /// `<project>-<name>` alone is ambiguous (`a-b`/`c` vs `a`/`b-c`), so a hash
    /// of the real identity always follows the readable part.
    pub fn compose_project(&self) -> String {
        let readable = sanitize(&format!("{}-{}", self.project, self.name)).to_lowercase();
        let readable = readable.trim_start_matches(['-', '_']);
        format!("{readable}-{:08x}", fnv32(format!("{}/{}", self.project, self.name).as_bytes()))
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

/// Every live workbench (see `Bench::is_live`). Unreadable metadata is
/// reported, not hidden.
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
                Ok(b) if b.is_live() => out.push(b),
                Ok(_) => {}
                Err(e) => eprintln!("wb: skipping unreadable {}: {e:#}", path.display()),
            }
        }
    }
    out.sort_by(|a, b| (&a.project, a.created).cmp(&(&b.project, b.created)));
    out
}

/// Records `b`, replacing whatever was recorded under its name. The write is
/// atomic: a crash can't leave half a file. Call under `lock()`.
pub fn save(b: &Bench) -> Result<()> {
    let p = meta_path(&b.project, &b.name);
    fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(b)?)?;
    fs::rename(&tmp, &p)?;
    Ok(())
}

/// Whether a live workbench already has this name. Call under `lock()`.
pub fn taken(project: &str, name: &str) -> bool {
    fs::read(meta_path(project, name))
        .ok()
        .and_then(|d| serde_json::from_slice::<Bench>(&d).ok())
        .is_some_and(|b| b.is_live())
}

/// Undoes a `wb new` that failed partway: runs `undo` and removes the record,
/// but only if the record is still this same unfinished creation. If someone
/// made a new workbench with the same name meanwhile, it must not be touched.
pub fn abandon(b: &Bench, undo: impl FnOnce() -> Result<()>) -> Result<()> {
    let _lock = lock()?;
    let ours = fs::read(meta_path(&b.project, &b.name))
        .ok()
        .and_then(|d| serde_json::from_slice::<Bench>(&d).ok())
        .is_some_and(|on_disk| on_disk.creating && on_disk.created == b.created && on_disk.port == b.port);
    if ours {
        undo()?;
        remove_record(b)?;
    }
    Ok(())
}

fn remove_record(b: &Bench) -> Result<()> {
    match fs::remove_file(meta_path(&b.project, &b.name)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
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
            creating: false,
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
    fn compose_names_are_readable_valid_and_distinct() {
        let names =
            [bench("app", "fix-a"), bench("app", "fix.a"), bench("App", "fix-a"), bench("a-b", "c"), bench("a", "b-c")]
                .map(|b| b.compose_project());
        assert!(names[0].starts_with("app-fix-a-"));
        for n in &names {
            assert!(n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'), "{n}");
        }
        for (i, a) in names.iter().enumerate() {
            assert!(names[i + 1..].iter().all(|b| b != a), "{a} collides");
        }
        assert!(bench("-app", "x").compose_project().starts_with("app-x-"));
    }

    #[test]
    fn a_record_counts_while_its_folder_exists_or_its_creation_is_recent() {
        let now = crate::util::now_secs();
        let dir = std::env::temp_dir();
        assert!(Bench { path: dir.clone(), ..bench("app", "a") }.is_live());
        assert!(
            !Bench { path: dir.join("no-such-folder-wb"), ..bench("app", "a") }.is_live(),
            "folder deleted by hand"
        );
        let creating = Bench { creating: true, created: now, path: dir.join("no-such-folder-wb"), ..bench("app", "a") };
        assert!(creating.is_live(), "copy under way");
        assert!(!Bench { created: now - 2 * ABANDONED_AFTER_SECS, ..creating }.is_live(), "interrupted long ago");
    }

    #[test]
    fn port_blocks_skip_other_benches() {
        let taken = [PORT_START, PORT_START + PORT_BLOCK];
        let p = allocate_port(&taken).unwrap();
        assert_eq!((p - PORT_START) % PORT_BLOCK, 0);
        assert!(taken.iter().all(|&t| p >= t + PORT_BLOCK || p + PORT_BLOCK <= t));
    }
}
