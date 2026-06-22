//! The supervisor: starts processes in dependency order, waits for health,
//! streams their output, and guarantees they are all torn down.

use crate::config::{Config, Process};
use crate::health::wait_healthy;
use crate::log::Logger;
use crate::proc::{killpg, spawn_in_own_group};
use crate::state::State;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Events flowing from worker threads / signal handler to the control loop.
pub enum Event {
    Healthy(String),
    HealthFailed(String, String),
    Exited {
        name: String,
        success: bool,
        desc: String,
    },
    /// A first SIGINT/SIGTERM: begin graceful shutdown.
    Signal,
}

/// Per-process bookkeeping shared with the signal handler (for force-kill).
pub struct Handle {
    pub pgid: i32,
    pub shutdown_signal: i32,
    pub exited: bool,
}

pub type Handles = Arc<Mutex<HashMap<String, Handle>>>;

pub struct Supervisor {
    cfg: Config,
    state: Arc<State>,
    log: Logger,
    grace: Duration,

    tx: Sender<Event>,
    rx: Receiver<Event>,
    cancel: Arc<AtomicBool>,
    procs: Handles,
    shutting_down: bool,
}

/// Outcome of processing a single event.
enum Control {
    Continue,
    /// Stop and shut down. `code` is the process exit code to return: 0 for a
    /// clean signal-initiated shutdown, non-zero when a process failed.
    Stop { reason: String, code: i32 },
}

impl Supervisor {
    pub fn new(cfg: Config, state: Arc<State>, log: Logger, grace: Duration) -> Supervisor {
        let (tx, rx) = mpsc::channel();
        Supervisor {
            cfg,
            state,
            log,
            grace,
            tx,
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            procs: Arc::new(Mutex::new(HashMap::new())),
            shutting_down: false,
        }
    }

    /// Clone of the event sender, for the signal-handling thread.
    pub fn event_sender(&self) -> Sender<Event> {
        self.tx.clone()
    }

    /// Shared process handles, for the force-kill path on a second signal.
    pub fn handles(&self) -> Handles {
        Arc::clone(&self.procs)
    }

    /// Start everything and supervise until shutdown. Returns a process exit code.
    pub fn run(&mut self) -> i32 {
        let order = match self.cfg.start_order() {
            Ok(o) => o,
            Err(e) => {
                self.log.system(&format!("config error: {e}"));
                return 1;
            }
        };
        let total = order.len();
        let processes = self.cfg.processes.clone();

        let mut healthy: HashSet<String> = HashSet::new();
        let mut exited: HashSet<String> = HashSet::new();

        // --- Startup: launch in dependency order, gating on each dep's health.
        for name in &order {
            let p = processes[name].clone();

            for dep in &p.depends_on {
                while !healthy.contains(dep) {
                    if exited.contains(dep) {
                        self.log.system(&format!(
                            "{dep:?} exited before becoming healthy; aborting startup"
                        ));
                        self.shutdown();
                        return 1;
                    }
                    match self.rx.recv() {
                        Ok(ev) => {
                            if let Control::Stop { reason, code } =
                                self.apply(ev, &mut healthy, &mut exited)
                            {
                                self.log.system(&format!("aborting startup: {reason}"));
                                self.shutdown();
                                return code;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }

            if let Err(e) = self.spawn_process(&p) {
                self.log.system(&format!("failed to start {name:?}: {e}"));
                self.shutdown();
                return 1;
            }
        }

        self.log.system(&format!("all {total} processes started"));

        // --- Supervise: wait for a signal or an unexpected exit.
        let mut announced = false;
        let mut exit_code = 0;
        while let Ok(ev) = self.rx.recv() {
            match self.apply(ev, &mut healthy, &mut exited) {
                Control::Continue => {
                    // Log the "fully up" milestone exactly once. `healthy` is the
                    // unified ready set: healthy services + completed oneshots.
                    if !announced && healthy.len() == total {
                        announced = true;
                        self.log.system("all processes ready");
                    }
                }
                Control::Stop { reason, code } => {
                    self.log.system(&format!("shutting down: {reason}"));
                    exit_code = code;
                    break;
                }
            }
        }

        self.shutdown();
        exit_code
    }

    /// Apply one event to our view of the world.
    fn apply(
        &self,
        ev: Event,
        healthy: &mut HashSet<String>,
        exited: &mut HashSet<String>,
    ) -> Control {
        match ev {
            Event::Healthy(name) => {
                self.log.system(&format!("{name} is ready"));
                healthy.insert(name);
                Control::Continue
            }
            Event::HealthFailed(name, err) => {
                self.log.system(&format!("{name} health check failed: {err}"));
                Control::Stop {
                    reason: format!("{name} failed its health check"),
                    code: 1,
                }
            }
            Event::Exited {
                name,
                success,
                desc,
            } => {
                exited.insert(name.clone());
                if let Some(h) = self.procs.lock().unwrap().get_mut(&name) {
                    h.exited = true;
                }
                let is_oneshot = self
                    .cfg
                    .processes
                    .get(&name)
                    .map(|p| p.oneshot)
                    .unwrap_or(false);

                if self.shutting_down {
                    Control::Continue
                } else if is_oneshot {
                    // A oneshot completing is success, not a crash.
                    if success {
                        self.log.system(&format!("{name} completed"));
                        healthy.insert(name); // mark ready so dependents unblock
                        Control::Continue
                    } else {
                        self.log.system(&format!("{name} failed ({desc})"));
                        Control::Stop {
                            reason: format!("oneshot {name} failed"),
                            code: 1,
                        }
                    }
                } else {
                    self.log.system(&format!("{name} exited ({desc})"));
                    Control::Stop {
                        reason: format!("{name} exited unexpectedly"),
                        code: 1,
                    }
                }
            }
            Event::Signal => Control::Stop {
                reason: "received termination signal".to_string(),
                code: 0,
            },
        }
    }

    fn spawn_process(&self, p: &Process) -> Result<(), String> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&p.command);
        if let Some(cwd) = &p.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &p.env {
            cmd.env(k, v);
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        spawn_in_own_group(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let pid = child.id() as i32;
        let pgid = pid; // setpgid(0,0) makes the child its own group leader

        self.state.record(&p.name, pid, pgid);
        self.procs.lock().unwrap().insert(
            p.name.clone(),
            Handle {
                pgid,
                shutdown_signal: parse_signal(p.shutdown_signal.as_deref()),
                exited: false,
            },
        );

        // Stream stdout and stderr, line by line, with a name prefix.
        if let Some(out) = child.stdout.take() {
            pump(out, p.name.clone(), self.log.clone());
        }
        if let Some(err) = child.stderr.take() {
            pump(err, p.name.clone(), self.log.clone());
        }

        // Health-check thread. A oneshot has no health check — its readiness is
        // signalled by a successful exit (handled in the reaper / apply()).
        if !p.oneshot {
            let tx = self.tx.clone();
            let cancel = Arc::clone(&self.cancel);
            let pc = p.clone();
            thread::spawn(move || match wait_healthy(&pc, &cancel) {
                Ok(()) => {
                    let _ = tx.send(Event::Healthy(pc.name));
                }
                Err(e) if e == "cancelled" => {}
                Err(e) => {
                    let _ = tx.send(Event::HealthFailed(pc.name, e));
                }
            });
        }

        // Reaper thread (owns the Child).
        {
            let tx = self.tx.clone();
            let state = Arc::clone(&self.state);
            let name = p.name.clone();
            thread::spawn(move || {
                let (success, desc) = match child.wait() {
                    Ok(status) => (status.success(), describe_status(status)),
                    Err(e) => (false, format!("wait failed: {e}")),
                };
                state.remove(&name);
                let _ = tx.send(Event::Exited {
                    name,
                    success,
                    desc,
                });
            });
        }

        self.log.system(&format!("started {} (pgid {pgid})", p.name));
        Ok(())
    }

    /// Terminate every still-running process group: signal, wait out the grace
    /// period, then SIGKILL survivors. Idempotent.
    fn shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        self.cancel.store(true, Ordering::SeqCst); // stop pending health checks

        // Snapshot the groups that haven't already exited.
        let pending: Vec<(String, i32, i32)> = {
            let procs = self.procs.lock().unwrap();
            procs
                .iter()
                .filter(|(_, h)| !h.exited)
                .map(|(n, h)| (n.clone(), h.pgid, h.shutdown_signal))
                .collect()
        };

        if pending.is_empty() {
            self.state.clear();
            return;
        }

        self.log.system(&format!(
            "stopping {} process(es), {}s grace before SIGKILL",
            pending.len(),
            self.grace.as_secs()
        ));

        // Phase 1: polite signal to every group.
        for (_, pgid, sig) in &pending {
            killpg(*pgid, *sig);
        }

        // Phase 2: wait for exits up to the grace deadline.
        let mut remaining: HashSet<String> = pending.iter().map(|(n, _, _)| n.clone()).collect();
        let deadline = Instant::now() + self.grace;
        while !remaining.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match self.rx.recv_timeout(deadline - now) {
                Ok(Event::Exited { name, .. }) => {
                    remaining.remove(&name);
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        // Phase 3: force-kill anything still alive.
        if !remaining.is_empty() {
            for (name, pgid, _) in &pending {
                if remaining.contains(name) {
                    self.log
                        .system(&format!("force-killing {name} (pgid {pgid})"));
                    killpg(*pgid, libc::SIGKILL);
                }
            }
            // Brief drain so we reap the killed children rather than leaving zombies.
            let drain_deadline = Instant::now() + Duration::from_secs(3);
            while !remaining.is_empty() {
                let now = Instant::now();
                if now >= drain_deadline {
                    break;
                }
                match self.rx.recv_timeout(drain_deadline - now) {
                    Ok(Event::Exited { name, .. }) => {
                        remaining.remove(&name);
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }

        self.state.clear();
        self.log.system("all processes stopped");
    }
}

/// Spawn a thread that forwards each line of `reader` to the logger.
fn pump<R: std::io::Read + Send + 'static>(reader: R, name: String, log: Logger) {
    thread::spawn(move || {
        let buf = BufReader::new(reader);
        for line in buf.lines() {
            match line {
                Ok(l) => log.line(&name, &l),
                Err(_) => break,
            }
        }
    });
}

fn describe_status(status: std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        if code == 0 {
            "exited cleanly".to_string()
        } else {
            format!("exit code {code}")
        }
    } else if let Some(sig) = status.signal() {
        format!("killed by signal {sig}")
    } else {
        "terminated".to_string()
    }
}

/// Map a signal name to its number. Unknown names fall back to SIGTERM.
pub fn parse_signal(name: Option<&str>) -> i32 {
    match name {
        Some("SIGINT") | Some("INT") => libc::SIGINT,
        Some("SIGKILL") | Some("KILL") => libc::SIGKILL,
        Some("SIGQUIT") | Some("QUIT") => libc::SIGQUIT,
        Some("SIGHUP") | Some("HUP") => libc::SIGHUP,
        Some("SIGTERM") | Some("TERM") | None => libc::SIGTERM,
        Some(_) => libc::SIGTERM,
    }
}
