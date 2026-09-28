//! Finding and stopping processes that run inside a workbench folder.

use std::collections::HashSet;
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System, UpdateKind};

pub fn snapshot() -> System {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cwd(UpdateKind::Always).with_exe(UpdateKind::Always),
    );
    sys
}

/// Our own process and every ancestor (the shell that ran `wb`, the terminal...).
/// Never kill those, even if the user ran `wb rm` from inside the folder.
fn protected(sys: &System) -> HashSet<Pid> {
    let mut out = HashSet::new();
    let mut cur = sysinfo::get_current_pid().ok();
    while let Some(pid) = cur {
        if !out.insert(pid) {
            break;
        }
        cur = sys.process(pid).and_then(|p| p.parent());
    }
    out
}

pub fn inside(sys: &System, dir: &Path) -> Vec<Pid> {
    let keep = protected(sys);
    sys.processes()
        .iter()
        .filter(|(pid, _)| !keep.contains(pid))
        .filter(|(_, p)| p.cwd().is_some_and(|c| c.starts_with(dir)) || p.exe().is_some_and(|e| e.starts_with(dir)))
        .map(|(pid, _)| *pid)
        .collect()
}

/// Interactive shells: someone's terminal tab `cd`'d into the folder. They
/// ignore SIGTERM, and SIGKILLing them would close that tab.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "nu", "pwsh", "powershell", "cmd"];

fn is_shell(p: &sysinfo::Process) -> bool {
    let name = p.name().to_string_lossy().to_lowercase();
    let name = name.trim_start_matches('-'); // login shells show as "-zsh"
    SHELLS.contains(&name.strip_suffix(".exe").unwrap_or(name))
}

pub struct Stopped {
    pub stopped: usize,
    /// Shells left running inside the folder.
    pub shells: usize,
}

/// Ask nicely, wait up to 3s, then force. Shells are left alone: stopping the
/// dev server inside one is enough, and the tab stays open.
pub fn stop_inside(dir: &Path) -> Stopped {
    let sys = snapshot();
    let all = inside(&sys, dir);
    let shells = all.iter().filter(|pid| sys.process(**pid).is_some_and(is_shell)).count();
    let targets = |sys: &System| -> Vec<Pid> {
        inside(sys, dir).into_iter().filter(|pid| !sys.process(*pid).is_some_and(is_shell)).collect()
    };
    let pids = targets(&sys);
    for pid in &pids {
        if let Some(p) = sys.process(*pid)
            && p.kill_with(Signal::Term).is_none()
        {
            p.kill();
        }
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let sys = snapshot();
        let alive = targets(&sys);
        if alive.is_empty() {
            break;
        }
        if Instant::now() + Duration::from_millis(200) >= deadline {
            for pid in alive {
                if let Some(p) = sys.process(pid) {
                    p.kill();
                }
            }
            break;
        }
        sleep(Duration::from_millis(200));
    }
    Stopped { stopped: pids.len(), shells }
}
