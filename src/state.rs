//! Persistent recovery state. Each spawned child is recorded to a JSON file the
//! moment it starts, so that a *fresh* run can clean up process groups orphaned
//! by a previously hard-killed supervisor.

use crate::log::Logger;
use crate::proc::{killpg, pgid_alive};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// One recorded child.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcState {
    pub name: String,
    pub pid: i32,
    pub pgid: i32,
    /// Process start time from /proc (clock ticks since boot). Guards against
    /// PID reuse: if the live PID's start time differs, it isn't our process.
    pub start_ticks: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Snapshot {
    supervisor_pid: i32,
    processes: BTreeMap<String, ProcState>,
}

/// Live, mutable handle to the on-disk state file.
pub struct State {
    path: PathBuf,
    inner: Mutex<Snapshot>,
}

impl State {
    pub fn new(path: impl Into<PathBuf>) -> State {
        State {
            path: path.into(),
            inner: Mutex::new(Snapshot {
                supervisor_pid: std::process::id() as i32,
                processes: BTreeMap::new(),
            }),
        }
    }

    /// Record a freshly spawned child and flush to disk.
    pub fn record(&self, name: &str, pid: i32, pgid: i32) {
        let mut snap = self.inner.lock().unwrap();
        snap.processes.insert(
            name.to_string(),
            ProcState {
                name: name.to_string(),
                pid,
                pgid,
                start_ticks: read_start_ticks(pid).unwrap_or(0),
            },
        );
        flush(&self.path, &snap);
    }

    /// Drop a child that has exited, and flush.
    pub fn remove(&self, name: &str) {
        let mut snap = self.inner.lock().unwrap();
        snap.processes.remove(name);
        flush(&self.path, &snap);
    }

    /// Delete the state file entirely (clean shutdown).
    pub fn clear(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Atomic write: temp file + rename, so a crash mid-write never corrupts state.
fn flush(path: &Path, snap: &Snapshot) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    let data = match serde_json::to_vec_pretty(snap) {
        Ok(d) => d,
        Err(_) => return,
    };
    if fs::write(&tmp, &data).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

/// Read a leftover state file and kill any process groups still alive. This is
/// the self-healing path for when a previous supervisor was SIGKILLed and never
/// got to clean up. Always removes the file afterward.
pub fn recover_orphans(path: &Path, log: &Logger) {
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(_) => return, // no leftover state
    };
    let snap: Snapshot = match serde_json::from_slice(&data) {
        Ok(s) => s,
        Err(e) => {
            log.system(&format!(
                "warning: ignoring unparseable state file {}: {e}",
                path.display()
            ));
            let _ = fs::remove_file(path);
            return;
        }
    };

    if snap.processes.is_empty() {
        let _ = fs::remove_file(path);
        return;
    }

    log.system(&format!(
        "found stale state from a previous run (supervisor pid {}); cleaning up {} process group(s)",
        snap.supervisor_pid,
        snap.processes.len()
    ));

    let mut to_kill: Vec<&ProcState> = Vec::new();
    for ps in snap.processes.values() {
        if ps.pgid <= 0 {
            continue;
        }
        // PID-reuse guard: if the live PID has a different start time, the
        // original process is long gone and this PID belongs to someone else.
        if ps.start_ticks != 0 {
            if let Some(cur) = read_start_ticks(ps.pid) {
                if cur != ps.start_ticks {
                    log.system(&format!(
                        "  {} (pid {}) was replaced by an unrelated process; skipping",
                        ps.name, ps.pid
                    ));
                    continue;
                }
            }
        }
        if pgid_alive(ps.pgid) {
            log.system(&format!(
                "  terminating orphaned group {:?} (pgid {})",
                ps.name, ps.pgid
            ));
            killpg(ps.pgid, libc::SIGTERM);
            to_kill.push(ps);
        }
    }

    if !to_kill.is_empty() {
        std::thread::sleep(Duration::from_secs(2));
        for ps in to_kill {
            if pgid_alive(ps.pgid) {
                log.system(&format!(
                    "  force-killing surviving group {:?} (pgid {})",
                    ps.name, ps.pgid
                ));
                killpg(ps.pgid, libc::SIGKILL);
            }
        }
    }

    let _ = fs::remove_file(path);
}

/// Read the process start time (field 22 of /proc/<pid>/stat) in clock ticks.
/// Returns None if it can't be read. Stable for the life of the process.
fn read_start_ticks(pid: i32) -> Option<u64> {
    let data = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm field (2) is wrapped in parens and may contain spaces/parens, so
    // split after the final ')'.
    let close = data.rfind(')')?;
    let after = data.get(close + 2..)?;
    // After comm, field 3 is state; the overall field 22 (starttime) is index
    // 22 - 3 = 19 in this post-comm slice.
    after.split_whitespace().nth(19)?.parse().ok()
}
