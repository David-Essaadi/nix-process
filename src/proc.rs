//! Low-level process-group primitives via libc.

use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// Configure a Command so the spawned child becomes the leader of a brand-new
/// process group (its pgid == its pid). This lets us signal the whole tree —
/// including any grandchildren — with a single `killpg`.
pub fn spawn_in_own_group(cmd: &mut Command) {
    // SAFETY: pre_exec runs in the forked child before exec. setpgid is
    // async-signal-safe and we touch no other shared state.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
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
