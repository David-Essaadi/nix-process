//! Readiness probes. `wait_healthy` blocks until a process's health check passes
//! or its timeout elapses.

use crate::config::{HealthCheck, Process};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Block until the process is healthy, the timeout expires, or `cancel` is set.
/// A process with no `health_check` is ready immediately.
///
/// Returns `Ok(())` when healthy, `Err(msg)` on timeout, and
/// `Err("cancelled")` if `cancel` fires first.
pub fn wait_healthy(p: &Process, cancel: &Arc<AtomicBool>) -> Result<(), String> {
    let hc = match &p.health_check {
        Some(hc) => hc,
        None => return Ok(()),
    };

    let timeout = Duration::from_secs(hc.timeout_seconds.unwrap_or(60));
    let interval = Duration::from_secs(hc.interval_seconds.unwrap_or(1).max(1));
    let deadline = Instant::now() + timeout;

    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".to_string());
        }
        match probe(hc) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "health check timed out after {}s (last error: {e})",
                        timeout.as_secs()
                    ));
                }
            }
        }
        // Sleep in small slices so cancellation is responsive.
        let mut slept = Duration::ZERO;
        while slept < interval {
            if cancel.load(Ordering::SeqCst) {
                return Err("cancelled".to_string());
            }
            std::thread::sleep(Duration::from_millis(100));
            slept += Duration::from_millis(100);
        }
    }
}

/// Run a single probe attempt.
fn probe(hc: &HealthCheck) -> Result<(), String> {
    if let Some(port) = hc.tcp_port {
        return probe_tcp(port);
    }
    if let Some(url) = &hc.http {
        return probe_http(url);
    }
    if let Some(cmd) = &hc.command {
        return probe_command(cmd);
    }
    // Empty health_check block: treat as ready.
    Ok(())
}

fn probe_tcp(port: u16) -> Result<(), String> {
    let addr = ("127.0.0.1", port)
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| "could not resolve 127.0.0.1".to_string())?;
    TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Minimal HTTP/1.0 GET so we don't pull in an HTTP-client dependency. Only
/// `http://host:port/path` URLs are supported; for HTTPS or anything fancier use
/// a `command` probe (e.g. curl).
fn probe_http(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("http probe only supports http:// URLs, got {url:?}"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|e| e.to_string())?),
        None => (authority, 80),
    };

    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| format!("could not resolve {host}"))?;
    let mut stream =
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;

    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let head = String::from_utf8_lossy(&buf);
    let status_line = head.lines().next().unwrap_or("");
    // Expect "HTTP/1.x <code> ..."
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| format!("bad HTTP response: {status_line:?}"))?;
    if code >= 400 {
        return Err(format!("HTTP status {code}"));
    }
    Ok(())
}

fn probe_command(cmd: &str) -> Result<(), String> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("command exited with {status}"))
    }
}
