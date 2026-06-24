//! PTY allocation + child spawn for the console, on `libc` (no
//! portable-pty / nix). `openpty` gives a master/slave pair; the
//! child runs with the slave as its controlling terminal and stdio,
//! the parent keeps the master and does all reads/writes/resizes
//! through it.
//!
//! Untested in isolation by design — the screen-level test in
//! `screen.rs` spawns a trivial child (`cat`/`printf`) through here
//! and asserts the round-trip, which exercises this whole file
//! without touching the clank binary.

use std::io;
use std::os::unix::io::RawFd;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};

/// Spawn `program args` in a fresh PTY of `rows`x`cols`, cwd `cwd`.
/// Returns the master fd (parent side) and the [`Child`]. The slave
/// is closed in the parent after spawn — only the child holds it, so
/// the master sees EOF the moment the child exits.
pub(crate) fn spawn(
    program: &str,
    args: &[String],
    cwd: &Path,
    rows: u16,
    cols: u16,
) -> io::Result<(RawFd, Child)> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let mut ws = winsize(rows, cols);
    // SAFETY: openpty fills both fds or returns non-zero; `ws` is a
    // valid initialized winsize and the name/termios args are null
    // (kernel defaults).
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(cwd);
    // `pre_exec` rewires stdio to the slave before exec, so the
    // inherited fds here are irrelevant — null them so nothing leaks
    // the parent's real terminal if pre_exec somehow bails early.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // SAFETY: the closure runs in the forked child between fork and
    // exec and calls only async-signal-safe libc functions. It opens
    // a new session, claims the slave as the controlling tty, wires
    // it to fds 0/1/2, then drops the spare slave + the master fd
    // (the child must not hold the master).
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(slave, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            for target in 0..=2 {
                if libc::dup2(slave, target) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            if slave > 2 {
                libc::close(slave);
            }
            libc::close(master);
            Ok(())
        });
    }

    let spawned = cmd.spawn();
    // The parent never reads or writes the slave; the child has its
    // own copy via the fork. Close ours regardless of spawn result.
    unsafe { libc::close(slave) };
    match spawned {
        Ok(child) => Ok((master, child)),
        Err(e) => {
            // Don't leak the master fd when the program is missing /
            // exec fails.
            unsafe { libc::close(master) };
            Err(e)
        }
    }
}

/// Resize a child's PTY. The kernel delivers SIGWINCH to the child's
/// process group, which repaints; the reader thread then refreshes
/// the grid. Best-effort — a failed resize just leaves the old size.
pub(crate) fn set_winsize(master: RawFd, rows: u16, cols: u16) {
    let ws = winsize(rows, cols);
    // SAFETY: `master` is a live pty master fd we own; `ws` is valid.
    unsafe {
        libc::ioctl(master, libc::TIOCSWINSZ, &ws);
    }
}

/// Write all `bytes` to the child, looping on short writes. Errors
/// (the child is gone) are dropped — the reader thread is the single
/// source of truth for child exit.
pub(crate) fn write_all(master: RawFd, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        // SAFETY: `master` is a live fd; we write from a valid slice.
        let n = unsafe { libc::write(master, bytes.as_ptr().cast(), bytes.len()) };
        if n <= 0 {
            break;
        }
        bytes = &bytes[n as usize..];
    }
}

/// Close a master fd. Called at teardown, after the child has been
/// reaped, so no reader thread is still blocked on it.
pub(crate) fn close(master: RawFd) {
    // SAFETY: `master` is a fd we own and stop using after this.
    unsafe { libc::close(master) };
}

fn winsize(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}
