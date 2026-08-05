//! Low-level process-group and pseudo-terminal primitives via libc.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// A freshly allocated pseudo-terminal pair. The child gets `slave` as all three
/// of its standard streams; we read its output from `master`.
pub struct Pty {
    pub master: OwnedFd,
    pub slave: OwnedFd,
}

/// Allocate a pty sized to `cols` x `rows`.
///
/// We give each process a pty rather than a pipe so that it sees a terminal:
/// most tooling checks `isatty` and silently drops its ANSI colouring (and
/// switches to 4KB block buffering) when it thinks it is writing to a file.
pub fn openpty(cols: u16, rows: u16) -> io::Result<Pty> {
    let mut master = 0;
    let mut slave = 0;
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty writes two valid fds through the out-params on success;
    // we pass a null name/termios (defaults) and a fully initialised winsize.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty succeeded, so both fds are freshly opened and owned by us.
    let pty = unsafe {
        Pty {
            master: OwnedFd::from_raw_fd(master),
            slave: OwnedFd::from_raw_fd(slave),
        }
    };

    // Mark both close-on-exec. Command dups the slave onto fds 0/1/2 (which
    // clears the flag on the dups), so the child still gets its terminal — but
    // without this, every later process would inherit stray master/slave fds and
    // hold this pty open, so we would never see EOF when this process exits.
    set_cloexec(&pty.master)?;
    set_cloexec(&pty.slave)?;
    Ok(pty)
}

fn set_cloexec(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: fd is a live descriptor we own for the duration of the call.
    let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Configure a Command to run in a brand-new session with `slave` as its
/// stdout/stderr.
///
/// stdin is deliberately `/dev/null`, not the terminal: a supervised process has
/// nobody to type at it, so a read on the pty would block forever. With
/// /dev/null a read returns EOF at once, so anything that prompts fails fast and
/// tools that probe `isatty(0)` correctly decide they are non-interactive.
/// `isatty` on stdout/stderr still holds, which is what drives colouring.
///
/// `setsid` also makes the child a process-group leader (pgid == pid), so a
/// single `killpg` still reaches the whole tree, grandchildren included.
///
/// We deliberately do NOT claim the pty as the session's controlling terminal
/// (`TIOCSCTTY`). Colouring only needs `isatty` on stdout/stderr, which the pty
/// already provides, so a controlling terminal buys nothing — and it re-arms the
/// job-control machinery we want to stay clear of:
///
/// * Once a session leader owns a controlling terminal, the kernel sends SIGHUP
///   to the foreground process group when that leader exits or the terminal
///   hangs up. Any program that re-execs, or forks a worker and lets the
///   original process go, then has its real work killed by SIGHUP. The Android
///   emulator launcher does exactly that, and died ~1ms after exec.
/// * With no controlling terminal, SIGTTIN/SIGTTOU cannot be raised at all —
///   the right posture for a process nobody can type at.
pub fn spawn_with_pty(cmd: &mut Command, slave: &OwnedFd) -> io::Result<()> {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(slave.try_clone()?));
    cmd.stderr(Stdio::from(slave.try_clone()?));

    // SAFETY: pre_exec runs in the forked child before exec. setsid is
    // async-signal-safe and we touch no other shared state.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

/// Send a signal to an entire process group (negative pid semantics).
pub fn killpg(pgid: i32, sig: libc::c_int) {
    // SAFETY: plain syscall; errors (e.g. ESRCH) are intentionally ignored.
    unsafe {
        libc::killpg(pgid, sig);
    }
}

/// Report whether any process in the group still exists, by sending signal 0.
pub fn pgid_alive(pgid: i32) -> bool {
    // SAFETY: signal 0 performs permission/existence checks without delivering.
    let rc = unsafe { libc::killpg(pgid, 0) };
    if rc == 0 {
        return true;
    }
    // EPERM means it exists but we may not signal it; ESRCH means it's gone.
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
