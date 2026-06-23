//! nix-process — run and supervise processes defined in a flake.nix, with no TUI.

mod config;
mod health;
mod log;
mod proc;
mod state;
mod supervisor;

use crate::config::{load_tests, Config};
use crate::log::Logger;
use crate::proc::killpg;
use crate::state::{recover_orphans, State};
use crate::supervisor::{Event, Supervisor};
use std::path::PathBuf;
use std::process::exit;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

struct Options {
    flake: String,
    attr: String,
    tests_attr: String,
    grace: Duration,
    state_path: PathBuf,
}

fn default_options() -> Options {
    Options {
        flake: ".".to_string(),
        attr: "processes".to_string(),
        tests_attr: "tests".to_string(),
        grace: Duration::from_secs(10),
        state_path: PathBuf::from(".nix-process/state.json"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("up") => cmd_up(&args[1..]),
        Some("test") => cmd_test(&args[1..]),
        Some("down") => cmd_down(&args[1..]),
        Some("-h") | Some("--help") | Some("help") | None => {
            print_usage();
            0
        }
        Some(other) => {
            eprintln!("nix-process: unknown command {other:?}\n");
            print_usage();
            2
        }
    };
    exit(code);
}

fn print_usage() {
    eprint!(
        "nix-process — run processes defined in a flake.nix\n\
\n\
Usage:\n\
  nix-process up [flags]          start all processes and supervise them\n\
  nix-process test <name> [flags] bring up a test's services, run it, tear down\n\
  nix-process down [flags]        clean up orphaned processes from a prior run\n\
\n\
Flags:\n\
  --flake <ref>      flake reference (default \".\")\n\
  --attr <attr>      attribute holding the process map (default \"processes\")\n\
  --tests-attr <a>   attribute holding the test map (default \"tests\")\n\
  --grace-seconds N  seconds to wait after SIGTERM before SIGKILL (default 10)\n\
  --state <path>     state file path (default \".nix-process/state.json\")\n"
    );
}

/// Consume the value following a flag at position `*i`, advancing `*i` past it.
fn flag_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("flag {flag} needs a value"))
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut opts = default_options();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--flake" => opts.flake = flag_value(args, &mut i, "--flake")?,
            "--attr" => opts.attr = flag_value(args, &mut i, "--attr")?,
            "--tests-attr" => opts.tests_attr = flag_value(args, &mut i, "--tests-attr")?,
            "--grace-seconds" => {
                let v = flag_value(args, &mut i, "--grace-seconds")?;
                opts.grace = Duration::from_secs(
                    v.parse()
                        .map_err(|_| format!("invalid --grace-seconds {v:?}"))?,
                );
            }
            "--state" => opts.state_path = PathBuf::from(flag_value(args, &mut i, "--state")?),
            other => return Err(format!("unknown flag {other:?}")),
        }
        i += 1;
    }
    Ok(opts)
}

fn cmd_up(args: &[String]) -> i32 {
    let opts = match parse_options(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("nix-process: {e}");
            return 2;
        }
    };
    let log = Logger::new();

    // Self-heal: reap orphans from any previously crashed run before starting new ones.
    recover_orphans(&opts.state_path, &log);

    log.system(&format!("evaluating {}#{}", opts.flake, opts.attr));
    let cfg = match Config::load(&opts.flake, &opts.attr) {
        Ok(c) => c,
        Err(e) => {
            log.system(&format!("error: {e}"));
            return 1;
        }
    };

    let state = Arc::new(State::new(opts.state_path.clone()));
    let mut sup = Supervisor::new(cfg, Arc::clone(&state), log.clone(), opts.grace);

    install_signal_handler(&sup, &log, Arc::clone(&state));

    sup.run()
}

fn cmd_test(args: &[String]) -> i32 {
    // First positional arg is the test name; the rest are flags.
    let name = match args.first() {
        Some(n) if !n.starts_with('-') => n.clone(),
        _ => {
            eprintln!("nix-process: `test` needs a test name, e.g. `nix-process test backend`");
            return 2;
        }
    };
    let opts = match parse_options(&args[1..]) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("nix-process: {e}");
            return 2;
        }
    };
    let log = Logger::new();

    // Self-heal: reap orphans from any previously crashed run first.
    recover_orphans(&opts.state_path, &log);

    let cfg = match Config::load(&opts.flake, &opts.attr) {
        Ok(c) => c,
        Err(e) => {
            log.system(&format!("error: {e}"));
            return 1;
        }
    };
    let tests = match load_tests(&opts.flake, &opts.tests_attr) {
        Ok(t) => t,
        Err(e) => {
            log.system(&format!("error: {e}"));
            return 1;
        }
    };
    let test = match tests.get(&name) {
        Some(t) => t.clone(),
        None => {
            let mut names: Vec<&String> = tests.keys().collect();
            names.sort();
            log.system(&format!("no test named {name:?}; defined tests: {names:?}"));
            return 1;
        }
    };

    // Bring up only the services this test needs, plus their transitive deps.
    let order = match cfg.start_order_from(&test.services) {
        Ok(o) => o,
        Err(e) => {
            log.system(&format!("error resolving services for test {name:?}: {e}"));
            return 1;
        }
    };

    let state = Arc::new(State::new(opts.state_path.clone()));
    let mut sup = Supervisor::new(cfg, Arc::clone(&state), log.clone(), opts.grace);
    install_signal_handler(&sup, &log, Arc::clone(&state));

    sup.run_test(order, &test)
}

fn cmd_down(args: &[String]) -> i32 {
    let opts = match parse_options(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("nix-process: {e}");
            return 2;
        }
    };
    let log = Logger::new();
    recover_orphans(&opts.state_path, &log);
    log.system("cleanup complete");
    0
}

/// First SIGINT/SIGTERM → graceful shutdown via an Event. A second one →
/// immediate force-kill of every group and a hard exit.
fn install_signal_handler(sup: &Supervisor, log: &Logger, state: Arc<State>) {
    let tx = sup.event_sender();
    let handles = sup.handles();
    let log = log.clone();

    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
    ])
    .expect("failed to install signal handler");

    thread::spawn(move || {
        let mut count = 0u32;
        for _sig in signals.forever() {
            count += 1;
            if count == 1 {
                let _ = tx.send(Event::Signal);
            } else {
                log.system("second signal — force-killing all process groups");
                for h in handles.lock().unwrap().values() {
                    killpg(h.pgid, libc::SIGKILL);
                }
                state.clear();
                exit(130);
            }
        }
    });
}
