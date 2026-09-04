//! Raw-terminal plumbing for `clank status --tui`.
//!
//! Hand-rolled on `libc` (no crossterm/termion): the alt-screen +
//! raw-mode lifecycle, the `TIOCGWINSZ` size probe, and the
//! flicker-free frame painter, with ONE async-signal-safe restore
//! path shared by Drop, the panic hook and the signal handler.
//!
//! Untested by design — every byte here touches a real terminal.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::sync::atomic::{AtomicPtr, Ordering};

/// (rows, cols) of the stdout tty via `TIOCGWINSZ`. libc carries
/// the correct per-platform request constant + struct layout —
/// hardcoding the number is the portability trap. Falls back to
/// 24x80 when stdout isn't a terminal (piped / headless tests).
pub(crate) fn term_size() -> (u16, u16) {
    use std::os::unix::io::AsRawFd;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let fd = std::io::stdout().as_raw_fd();
    // SAFETY: ws is a valid zeroed winsize; ioctl fills it or
    // returns -1.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
        (ws.ws_row, ws.ws_col)
    } else {
        (24, 80)
    }
}

/// (rows, cols) of an arbitrary tty device via `TIOCGWINSZ`, e.g.
/// `/dev/ttys004` — used to measure the REAL window from inside a
/// zellij pane by asking the attached client's controlling tty
/// (zellij-in-session-orientation). `None` on open/ioctl failure or
/// degenerate sizes; `O_NONBLOCK` so a wedged tty can't hang us,
/// `O_NOCTTY` so we never adopt it as our controlling terminal.
pub(crate) fn winsize_of_tty(dev: &str) -> Option<(u16, u16)> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(dev)
        .ok()?;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: ws is a valid zeroed winsize; ioctl fills it or
    // returns -1.
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
        Some((ws.ws_row, ws.ws_col))
    } else {
        None
    }
}

const ENTER_SEQ: &str = "\x1b[?1049h\x1b[?25l"; // alt-screen + hide cursor
// Restore: leave mouse modes, show cursor, leave alt-screen. Nothing
// here enables the mouse any more, but the disable STAYS: this is the
// single signal-safe restore path, and it must be safe to fire from a
// handler regardless of what state the terminal was found in.
const RESTORE_SEQ: &str = "\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?25h\x1b[?1049l";

/// RAII alt-screen guard. `Drop` restores on every normal exit
/// path; a panic hook covers `panic=abort` (where Drop won't run);
/// a SIGINT/SIGTERM handler covers Ctrl-C in a direct terminal
/// (without it the user's terminal is left wedged on alt-screen with
/// a hidden cursor).
pub(crate) struct AltScreen {
    /// The cooked-mode termios to restore on exit.
    orig: libc::termios,
}

/// The original termios as a leaked pointer so the (async-signal-
/// safe) SIGINT/SIGTERM handler can restore it without touching a
/// `static mut` (banned refs in edition 2024) or allocating.
static TERMIOS_PTR: AtomicPtr<libc::termios> = AtomicPtr::new(std::ptr::null_mut());

impl AltScreen {
    /// ICANON+ECHO off so keystrokes are read (not echoed), but ISIG
    /// kept so Ctrl-C still exits the view.
    pub(crate) fn enter() -> Self {
        // VMIN=1 so a stdin read blocks until ≥1 byte (the kernel
        // notification the reader thread waits on — no polling).
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        unsafe {
            libc::tcgetattr(libc::STDIN_FILENO, &mut orig);
            TERMIOS_PTR.store(Box::into_raw(Box::new(orig)), Ordering::Relaxed);
            let mut raw = orig;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
        }

        print!("{ENTER_SEQ}");
        let _ = std::io::stdout().flush();

        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &orig) };
            let mut out = std::io::stdout();
            let _ = out.write_all(RESTORE_SEQ.as_bytes());
            let _ = out.flush();
            prev(info);
        }));

        // SAFETY: installing a handler that only calls
        // async-signal-safe functions (tcsetattr, write, _exit).
        let handler =
            restore_and_exit as extern "C" fn(libc::c_int) as *const () as libc::sighandler_t;
        unsafe {
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGTERM, handler);
        }
        AltScreen { orig }
    }
}

impl Drop for AltScreen {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.orig) };
        print!("{RESTORE_SEQ}");
        let _ = std::io::stdout().flush();
    }
}

extern "C" fn restore_and_exit(sig: libc::c_int) {
    // Mirrors RESTORE_SEQ: disable mouse modes FIRST, then cursor +
    // alt-screen. Kept in sync by hand (this is a byte literal for the
    // async-signal-safe path).
    const RESTORE: &[u8] = b"\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?25h\x1b[?1049l";
    // SAFETY: tcsetattr, write, _exit are async-signal-safe; the ptr is
    // a leaked Box set once in `enter`, read atomically here.
    unsafe {
        let p = TERMIOS_PTR.load(Ordering::Relaxed);
        if !p.is_null() {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, p);
        }
        libc::write(1, RESTORE.as_ptr().cast(), RESTORE.len());
        libc::_exit(128 + sig);
    }
}

/// One frame: cursor home, each line + clear-to-EOL, then clear
/// any leftover rows from a taller previous frame. No full-screen
/// clear → no flicker.
pub(crate) fn paint(lines: &[String]) {
    let mut buf = String::from("\x1b[H");
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            buf.push_str("\r\n");
        }
        buf.push_str(line);
        buf.push_str("\x1b[K");
    }
    buf.push_str("\x1b[J");
    let mut out = std::io::stdout();
    let _ = out.write_all(buf.as_bytes());
    let _ = out.flush();
}
