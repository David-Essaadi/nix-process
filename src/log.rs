//! Thread-safe, name-prefixed terminal logging. No TUI — just interleaved lines
//! written straight to stdout, colored per process when stdout is a TTY.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[90m";
const PALETTE: &[&str] = &[
    "\x1b[36m", "\x1b[32m", "\x1b[33m", "\x1b[35m", "\x1b[34m", "\x1b[31m", "\x1b[96m", "\x1b[92m",
];

const PREFIX_WIDTH: usize = 12;
const SYSTEM_NAME: &str = "nix-process";

#[derive(Clone)]
pub struct Logger {
    inner: Arc<Mutex<Inner>>,
    colors: bool,
}

struct Inner {
    assigned: HashMap<String, &'static str>,
    next: usize,
}

impl Logger {
    pub fn new() -> Logger {
        Logger {
            inner: Arc::new(Mutex::new(Inner {
                assigned: HashMap::new(),
                next: 0,
            })),
            colors: stdout_is_tty(),
        }
    }

    fn color_for(&self, name: &str) -> &'static str {
        let mut inner = self.inner.lock().unwrap();
        if let Some(c) = inner.assigned.get(name) {
            return c;
        }
        let c = PALETTE[inner.next % PALETTE.len()];
        inner.next += 1;
        inner.assigned.insert(name.to_string(), c);
        c
    }

    /// Print one line of process output.
    pub fn line(&self, name: &str, line: &str) {
        let prefix = pad(name);
        let stdout = std::io::stdout();
        let mut h = stdout.lock();
        if self.colors {
            let color = self.color_for(name);
            let _ = writeln!(h, "{color}{prefix}{RESET} | {line}");
        } else {
            let _ = writeln!(h, "{prefix} | {line}");
        }
    }

    /// Print a supervisor-level message (distinct from process output).
    pub fn system(&self, msg: &str) {
        let prefix = pad(SYSTEM_NAME);
        let stdout = std::io::stdout();
        let mut h = stdout.lock();
        if self.colors {
            let _ = writeln!(h, "{DIM}{prefix}{RESET} | {DIM}{msg}{RESET}");
        } else {
            let _ = writeln!(h, "{prefix} | {msg}");
        }
    }
}

/// Left-pad / truncate a name to the fixed prefix width.
fn pad(name: &str) -> String {
    let w = PREFIX_WIDTH;
    if name.len() >= w {
        // Truncate with an ellipsis marker, keeping width.
        format!("{}…", &name[..w - 1])
    } else {
        format!("{name:<w$}")
    }
}

fn stdout_is_tty() -> bool {
    // SAFETY: isatty just inspects a file descriptor.
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}
