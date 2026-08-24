//! Configuration: the `processes` attribute evaluated out of the user's flake,
//! plus dependency ordering.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::process::Command;

/// A single service definition.
#[derive(Debug, Clone, Deserialize)]
pub struct Process {
    /// Filled in from the map key after deserialization.
    #[serde(skip)]
    pub name: String,

    /// Shell command, run via `sh -c`.
    pub command: String,

    /// Working directory (optional).
    #[serde(default)]
    pub cwd: Option<String>,

    /// Extra environment variables (optional), layered over the inherited env.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Names of processes that must be ready before this one starts.
    #[serde(default)]
    pub depends_on: Vec<String>,

    /// A oneshot runs to completion: its successful exit (code 0) is its "ready"
    /// signal (it does not run a health check), and exiting is not treated as a
    /// crash. A non-zero exit is fatal. Use for setup/migration tasks.
    #[serde(default)]
    pub oneshot: bool,

    /// Optional override for the signal sent on shutdown (default SIGTERM).
    #[serde(default)]
    pub shutdown_signal: Option<String>,

    /// Optional readiness probe.
    #[serde(default)]
    pub health_check: Option<HealthCheck>,
}

/// How to decide a process is "ready". At most one of `tcp_port` / `http` /
/// `command` should be set; if none are, the process is ready immediately.
#[derive(Debug, Clone, Deserialize)]
pub struct HealthCheck {
    #[serde(default)]
    pub tcp_port: Option<u16>,
    #[serde(default)]
    pub http: Option<String>,
    #[serde(default)]
    pub command: Option<String>,

    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub interval_seconds: Option<u64>,
}

/// The whole config: just the process map (global tunables live on the CLI).
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub processes: BTreeMap<String, Process>,
}

/// A declared one-off command (a "test") plus the services it needs running.
/// nix-process brings up the transitive closure of `services`, runs `command`,
/// then tears the services down and returns the command's exit code.
#[derive(Debug, Clone, Deserialize)]
pub struct TestSpec {
    #[serde(skip)]
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Evaluate `nix eval <flake>#<attr> --json` and return the raw JSON bytes.
///
/// The eval is `--impure` so process/test definitions can branch on the
/// environment — e.g. `builtins.getEnv "CI"` to add CI-only services, or reading
/// env to configure docker services. Flake inputs are still locked; impurity only
/// unlocks env / `builtins.currentSystem` reads.
fn nix_eval_json(flake: &str, attr: &str) -> Result<Vec<u8>, String> {
    let reference = format!("{flake}#{attr}");
    let out = Command::new("nix")
        .args(["eval", &reference, "--json", "--impure"])
        .output()
        .map_err(|e| format!("failed to run `nix eval`: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("`nix eval {reference}` failed:\n{}", stderr.trim()));
    }
    Ok(out.stdout)
}

/// Evaluate `nix eval <flake>#<attr> --json` into the map of declared tests.
pub fn load_tests(flake: &str, attr: &str) -> Result<BTreeMap<String, TestSpec>, String> {
    let data = nix_eval_json(flake, attr)?;
    let mut tests: BTreeMap<String, TestSpec> = serde_json::from_slice(&data)
        .map_err(|e| format!("parsing `{flake}#{attr}` as a test map: {e}"))?;
    for (name, t) in tests.iter_mut() {
        t.name = name.clone();
        if t.command.trim().is_empty() {
            return Err(format!("test {name:?} has an empty command"));
        }
    }
    Ok(tests)
}

impl Config {
    /// Evaluate `nix eval <flake>#<attr> --json` and parse it.
    pub fn load(flake: &str, attr: &str) -> Result<Config, String> {
        let data = nix_eval_json(flake, attr)?;
        // The attribute itself is the process map.
        let processes: BTreeMap<String, Process> = serde_json::from_slice(&data)
            .map_err(|e| format!("parsing `{flake}#{attr}` as a process map: {e}"))?;
        Config::from_processes(processes)
    }

    /// Build (and validate) a Config from a raw process map.
    pub fn from_processes(mut processes: BTreeMap<String, Process>) -> Result<Config, String> {
        if processes.is_empty() {
            return Err("config defines no processes".to_string());
        }
        for (name, p) in processes.iter_mut() {
            p.name = name.clone();
            if p.command.trim().is_empty() {
                return Err(format!("process {name:?} has an empty command"));
            }
        }
        let cfg = Config { processes };
        // Validate the dependency graph eagerly so failures surface up-front.
        cfg.start_order()?;
        Ok(cfg)
    }

    /// Return all process names in a valid start order honoring `depends_on`.
    /// Errors on cycles or references to undefined processes. Deterministic
    /// because `processes` is a BTreeMap (sorted keys).
    pub fn start_order(&self) -> Result<Vec<String>, String> {
        let all: Vec<String> = self.processes.keys().cloned().collect();
        self.start_order_from(&all)
    }

    /// Return the deps-first start order of `roots` plus their transitive
    /// `depends_on` closure — i.e. exactly the processes needed to bring `roots`
    /// up, nothing else. Deterministic and cycle/undefined-dep checked.
    pub fn start_order_from(&self, roots: &[String]) -> Result<Vec<String>, String> {
        let mut marks: BTreeMap<&str, Mark> = BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        for root in roots {
            if !self.processes.contains_key(root) {
                return Err(format!("no such process {root:?}"));
            }
            self.visit(root, &mut marks, &mut order)?;
        }
        Ok(order)
    }

    fn visit<'a>(
        &'a self,
        node: &'a str,
        marks: &mut BTreeMap<&'a str, Mark>,
        order: &mut Vec<String>,
    ) -> Result<(), String> {
        match marks.get(node) {
            Some(Mark::Done) => return Ok(()),
            Some(Mark::InProgress) => {
                return Err(format!("dependency cycle detected involving {node:?}"))
            }
            None => {}
        }
        let proc = self
            .processes
            .get(node)
            .ok_or_else(|| format!("process {node:?} (referenced in depends_on) is not defined"))?;

        marks.insert(node, Mark::InProgress);
        for dep in &proc.depends_on {
            self.visit(dep, marks, order)?;
        }
        marks.insert(node, Mark::Done);
        order.push(node.to_string());
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    InProgress,
    Done,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a Config from a JSON process map, exercising the same parse path
    /// as `Config::load`. Graph: ticker -> web -> worker (worker depends on web,
    /// web depends on ticker).
    fn sample() -> Config {
        let json = r#"{
            "ticker": { "command": "tick" },
            "web":    { "command": "serve", "depends_on": ["ticker"] },
            "worker": { "command": "work",  "depends_on": ["web"] }
        }"#;
        let processes: BTreeMap<String, Process> = serde_json::from_str(json).unwrap();
        Config::from_processes(processes).unwrap()
    }

    #[test]
    fn subset_pulls_in_transitive_deps_in_order() {
        let cfg = sample();
        assert_eq!(
            cfg.start_order_from(&["worker".to_string()]).unwrap(),
            vec!["ticker", "web", "worker"]
        );
        assert_eq!(
            cfg.start_order_from(&["web".to_string()]).unwrap(),
            vec!["ticker", "web"]
        );
    }

    #[test]
    fn leaf_target_starts_only_itself() {
        let cfg = sample();
        assert_eq!(
            cfg.start_order_from(&["ticker".to_string()]).unwrap(),
            vec!["ticker"]
        );
    }

    #[test]
    fn undefined_target_errors() {
        let cfg = sample();
        let err = cfg.start_order_from(&["nope".to_string()]).unwrap_err();
        assert!(err.contains("nope"), "unexpected error: {err}");
    }
}
