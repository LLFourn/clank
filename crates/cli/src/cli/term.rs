//! Shared raw-terminal plumbing for the full-screen TUIs (`clank
//! status --tui` and the `clank open` console).
//!
//! Hand-rolled on `libc` (no crossterm/termion): the alt-screen +
//! raw-mode lifecycle, the `TIOCGWINSZ` size probe, and the
//! flicker-free frame painter. Factored out of `status_tui` so the
//! console reuses the *same* async-signal-safe restore path rather
//! than forking a second copy of it (a forked teardown is two places
//! to get the SIGINT/panic restore right).
//!
//! Untested by design — every byte here touches a real terminal.

use std::io::Write as _;
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

const ENTER_SEQ: &str = "\x1b[?1049h\x1b[?25l"; // alt-screen + hide cursor
// SGR mouse reporting (button + drag) for the console's own selection.
const MOUSE_ON: &str = "\x1b[?1002h\x1b[?1006h";
// Restore: leave mouse modes (harmless if never enabled), show cursor,
// leave alt-screen. Mouse-disable is FIRST and lives here — the single
// signal-safe restore path (Drop + panic hook + signal handler) — so a
// crash never leaves the terminal spewing mouse escapes.
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

/// How aggressively to put the terminal into raw mode.
enum RawLevel {
    /// ICANON+ECHO off, signals (Ctrl-C) still act locally.
    CookedSignals,
    /// Everything off (cfmakeraw) — bytes pass straight through.
    Full,
}

impl AltScreen {
    /// Status-view raw mode: ICANON+ECHO off so keystrokes are read
    /// (not echoed), but ISIG kept so Ctrl-C still exits the
    /// read-only view. For `status --tui`.
    pub(crate) fn enter() -> Self {
        Self::enter_with(RawLevel::CookedSignals)
    }

    /// Full raw mode (cfmakeraw): ISIG/IXON/ICRNL/OPOST off too, so
    /// EVERY byte — including Ctrl-C — is forwarded verbatim to the
    /// active child instead of acting on the console itself. For the
    /// console, which is a transparent multiplexer.
    pub(crate) fn enter_raw() -> Self {
        Self::enter_with(RawLevel::Full)
    }

    fn enter_with(level: RawLevel) -> Self {
        // VMIN=1 so a stdin read blocks until ≥1 byte (the kernel
        // notification the reader thread waits on — no polling).
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        unsafe {
            libc::tcgetattr(libc::STDIN_FILENO, &mut orig);
            TERMIOS_PTR.store(Box::into_raw(Box::new(orig)), Ordering::Relaxed);
            let mut raw = orig;
            match level {
                RawLevel::CookedSignals => raw.c_lflag &= !(libc::ICANON | libc::ECHO),
                RawLevel::Full => libc::cfmakeraw(&mut raw),
            }
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
        }

        print!("{ENTER_SEQ}");
        // Full raw = the console: it owns the mouse for pane-aware
        // selection. (status --tui, CookedSignals, leaves it to the
        // terminal.) The matching disable is in RESTORE_SEQ.
        if matches!(level, RawLevel::Full) {
            print!("{MOUSE_ON}");
        }
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
