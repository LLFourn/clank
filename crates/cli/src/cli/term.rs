//! Raw-terminal plumbing for `clank tui`.
//!
//! Hand-rolled (no crossterm/termion): the alt-screen + raw-mode
//! lifecycle, the `TIOCGWINSZ` size probe, and the flicker-free frame
//! painter, with ONE async-signal-safe restore path shared by Drop, the
//! panic hook and the signal handler.

use std::io::Write as _;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::OpenOptionsExt as _;

use rustix::termios::{LocalModes, OptionalActions, SpecialCodeIndex, Termios};

/// (rows, cols) of the stdout tty via `TIOCGWINSZ`. Falls back to
/// 24x80 when stdout isn't a terminal (piped / headless tests).
pub(crate) fn term_size() -> (u16, u16) {
    winsize(std::io::stdout()).unwrap_or((24, 80))
}

/// (rows, cols) of an arbitrary tty device via `TIOCGWINSZ`, e.g.
/// `/dev/ttys004` — used to measure the REAL window from inside a
/// zellij pane by asking the attached client's controlling tty
/// (zellij-in-session-orientation). `None` on open/ioctl failure or
/// degenerate sizes; `O_NONBLOCK` so a wedged tty can't hang us,
/// `O_NOCTTY` so we never adopt it as our controlling terminal.
pub(crate) fn winsize_of_tty(dev: &str) -> Option<(u16, u16)> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(dev)
        .ok()?;
    winsize(&f)
}

fn winsize(tty: impl AsFd) -> Option<(u16, u16)> {
    let ws = rustix::termios::tcgetwinsize(tty).ok()?;
    (ws.ws_row > 0 && ws.ws_col > 0).then_some((ws.ws_row, ws.ws_col))
}

const ENTER_SEQ: &str = "\x1b[?1049h\x1b[?25l"; // alt-screen + hide cursor
// Restore: leave mouse modes, show cursor, leave alt-screen. Nothing
// here enables the mouse any more, but the disable STAYS: this is the
// single signal-safe restore path, and it must be safe to fire from a
// handler regardless of what state the terminal was found in.
const RESTORE_SEQ: &str = "\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?25h\x1b[?1049l";

/// ICANON+ECHO off on `tty`, so keystrokes are read (not echoed), but
/// ISIG kept so Ctrl-C still exits the view. Drop puts back exactly the
/// termios it found.
struct RawMode<Fd: AsFd> {
    tty: Fd,
    orig: Termios,
}

impl<Fd: AsFd> RawMode<Fd> {
    fn enter(tty: Fd) -> rustix::io::Result<Self> {
        let orig = rustix::termios::tcgetattr(&tty)?;
        let mut raw = orig.clone();
        raw.local_modes -= LocalModes::ICANON | LocalModes::ECHO;
        // VMIN=1 so a stdin read blocks until ≥1 byte (the kernel
        // notification the reader thread waits on — no polling).
        raw.special_codes[SpecialCodeIndex::VMIN] = 1;
        raw.special_codes[SpecialCodeIndex::VTIME] = 0;
        rustix::termios::tcsetattr(&tty, OptionalActions::Now, &raw)?;
        Ok(RawMode { tty, orig })
    }
}

impl<Fd: AsFd> Drop for RawMode<Fd> {
    fn drop(&mut self) {
        let _ = rustix::termios::tcsetattr(&self.tty, OptionalActions::Now, &self.orig);
    }
}

/// RAII alt-screen guard. `Drop` restores on every normal exit
/// path; a panic hook covers `panic=abort` (where Drop won't run);
/// a SIGINT/SIGTERM handler covers Ctrl-C in a direct terminal
/// (without it the user's terminal is left wedged on alt-screen with
/// a hidden cursor).
pub(crate) struct AltScreen {
    /// `None` when stdin is not a terminal, so there is no mode to set.
    raw: Option<RawMode<BorrowedFd<'static>>>,
}

impl AltScreen {
    pub(crate) fn enter() -> Self {
        let raw = RawMode::enter(rustix::stdio::stdin()).ok();
        let orig = raw.as_ref().map(|raw| raw.orig.clone());
        if let Some(orig) = &orig {
            restore::save(orig.clone());
        }

        print!("{ENTER_SEQ}");
        let _ = std::io::stdout().flush();

        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Some(orig) = &orig {
                let _ =
                    rustix::termios::tcsetattr(rustix::stdio::stdin(), OptionalActions::Now, orig);
            }
            let mut out = std::io::stdout();
            let _ = out.write_all(RESTORE_SEQ.as_bytes());
            let _ = out.flush();
            prev(info);
        }));

        restore::install(restore::HANDLED);
        AltScreen { raw }
    }
}

impl Drop for AltScreen {
    fn drop(&mut self) {
        drop(self.raw.take());
        print!("{RESTORE_SEQ}");
        let _ = std::io::stdout().flush();
    }
}

/// Put `tty` back to `saved` and write the restore sequence to `out`.
///
/// Async-signal-safe: the signal handler runs it. Each call is a single
/// syscall wrapper that neither allocates nor locks, and the bytes are a
/// constant.
fn restore_terminal(tty: BorrowedFd<'_>, saved: Option<&Termios>, out: BorrowedFd<'_>) {
    if let Some(saved) = saved {
        let _ = rustix::termios::tcsetattr(tty, OptionalActions::Now, saved);
    }
    let _ = rustix::io::write(out, RESTORE_SEQ.as_bytes());
}

/// The SIGINT/SIGTERM handler and the termios it restores.
///
/// Unsafe because a C signal handler can only reach its state through a
/// static, and neither std nor rustix installs one or offers `_exit`.
#[allow(unsafe_code)]
mod restore {
    use std::sync::atomic::{AtomicPtr, Ordering};

    use rustix::termios::Termios;

    pub(super) const HANDLED: &[libc::c_int] = &[libc::SIGINT, libc::SIGTERM];

    /// A leaked pointer rather than a lock, so the handler can read it
    /// without allocating or blocking.
    static SAVED: AtomicPtr<Termios> = AtomicPtr::new(std::ptr::null_mut());

    pub(super) fn save(orig: Termios) {
        SAVED.store(Box::into_raw(Box::new(orig)), Ordering::Release);
    }

    fn saved() -> Option<&'static Termios> {
        // SAFETY: SAVED is null or a pointer from `Box::into_raw` that is
        // never freed or written through, so it is valid for 'static. The
        // Box is fully written before the Release store publishes it, and
        // this Acquire load synchronises with that store, so a handler on
        // any thread reads an initialised termios.
        unsafe { SAVED.load(Ordering::Acquire).as_ref() }
    }

    extern "C" fn restore_and_exit(sig: libc::c_int) {
        super::restore_terminal(rustix::stdio::stdin(), saved(), rustix::stdio::stdout());
        // SAFETY: `_exit` is async-signal-safe and skips the atexit
        // handlers and destructors that would not be.
        unsafe { libc::_exit(128 + sig) }
    }

    fn handler() -> libc::sighandler_t {
        restore_and_exit as extern "C" fn(libc::c_int) as *const () as libc::sighandler_t
    }

    pub(super) fn install(signals: &[libc::c_int]) {
        for &sig in signals {
            // SAFETY: the handler calls only async-signal-safe code
            // (`restore_terminal` and `_exit`), and `signal` takes no
            // pointers but the handler's address.
            unsafe {
                libc::signal(sig, handler());
            }
        }
    }

    #[cfg(test)]
    mod tests {
        /// A signal nothing in clank handles.
        const SPARE: libc::c_int = libc::SIGUSR2;

        fn disposition(sig: libc::c_int) -> libc::sigaction {
            // SAFETY: an all-zero `sigaction` is a valid value of the C
            // struct, and a null new action only reads the current one
            // into `old`, which outlives the call.
            unsafe {
                let mut old: libc::sigaction = std::mem::zeroed();
                assert_eq!(libc::sigaction(sig, std::ptr::null(), &mut old), 0);
                old
            }
        }

        fn signal(sig: libc::c_int, to: libc::sighandler_t) {
            // SAFETY: as in `install`; no signal is delivered.
            unsafe {
                libc::signal(sig, to);
            }
        }

        /// The signals a mask holds. A raw byte compare would also compare
        /// the part of glibc's 1024-bit `sigset_t` the kernel never fills.
        fn members(mask: &libc::sigset_t) -> Vec<libc::c_int> {
            (1..=64)
                .filter(|&sig| {
                    // SAFETY: `mask` is a valid `sigset_t` for the call, and
                    // `sigismember` only reads it.
                    unsafe { libc::sigismember(mask, sig) == 1 }
                })
                .collect()
        }

        /// Installation keeps `signal`'s semantics, restart included,
        /// read back without delivering anything. It runs in a fresh
        /// process of this test binary, so the harness's own dispositions
        /// are never touched.
        #[test]
        fn the_handler_is_installed_as_signal_would_install_it() {
            use crate::proc_run::tests::{in_child, is_child};
            const INSTALLED: &str = "installed as signal installs it";
            if !is_child() {
                assert_eq!(
                    in_child(
                        "cli::term::restore::tests::the_handler_is_installed_as_signal_would_install_it",
                        &[],
                    ),
                    INSTALLED
                );
                return;
            }
            assert_eq!(super::HANDLED, &[libc::SIGINT, libc::SIGTERM]);

            super::install(&[SPARE]);
            let installed = disposition(SPARE);
            signal(SPARE, libc::SIG_DFL);
            signal(SPARE, super::handler());
            let by_signal = disposition(SPARE);

            assert_eq!(installed.sa_sigaction, super::handler());
            assert_ne!(
                installed.sa_flags & libc::SA_RESTART,
                0,
                "interrupted reads restart"
            );
            assert_eq!(installed.sa_flags, by_signal.sa_flags);
            assert_eq!(members(&installed.sa_mask), members(&by_signal.sa_mask));
            println!("RESULT:{INSTALLED}");
        }
    }
}

/// One frame: cursor home, each line + clear-to-EOL, then clear
/// any leftover rows from a taller previous frame. No full-screen
/// clear → no flicker.
/// The OSC 52 sequence that puts `text` on the clipboard of whatever
/// terminal is really in front of the user.
///
/// It is the only path that survives the trip: clank runs inside
/// zellij, often over ssh, so a local clipboard call would write to
/// the wrong machine's clipboard, or to none. The terminal never
/// acknowledges the write, so nothing downstream can wait on it.
pub(crate) fn osc52(text: &str) -> String {
    use base64::Engine as _;
    format!(
        "\x1b]52;c;{}\x07",
        base64::engine::general_purpose::STANDARD.encode(text)
    )
}

/// Put `text` on the terminal's clipboard.
pub(crate) fn copy(text: &str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(osc52(text).as_bytes());
    let _ = out.flush();
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::pty::OpenptFlags;
    use std::os::fd::OwnedFd;
    use std::os::unix::ffi::OsStrExt;

    /// A pseudo-terminal standing in for the user's: (controller, tty).
    fn pty() -> (OwnedFd, std::fs::File) {
        let controller = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        rustix::pty::grantpt(&controller).unwrap();
        rustix::pty::unlockpt(&controller).unwrap();
        let name = rustix::pty::ptsname(&controller, Vec::new()).unwrap();
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(std::ffi::OsStr::from_bytes(name.as_bytes()))
            .unwrap();
        (controller, tty)
    }

    fn modes(tty: &std::fs::File) -> Termios {
        rustix::termios::tcgetattr(tty).unwrap()
    }

    /// Equal in every setting. `PENDIN` is left out: Darwin sets it
    /// itself when canonical mode comes back, as a note that pending
    /// input is to be reprinted, so it is the kernel's state rather
    /// than anything a restore wrote.
    fn same(a: &Termios, b: &Termios) -> bool {
        let settings = |t: &Termios| {
            let mut t = t.clone();
            t.local_modes -= LocalModes::PENDIN;
            format!("{t:?}")
        };
        settings(a) == settings(b)
    }

    #[test]
    fn raw_mode_stops_echo_and_line_editing_and_nothing_else() {
        let (_controller, tty) = pty();
        let before = modes(&tty);
        assert!(
            before
                .local_modes
                .contains(LocalModes::ICANON | LocalModes::ECHO)
        );

        let _raw = RawMode::enter(&tty).unwrap();
        let raw = modes(&tty);
        assert!(
            !raw.local_modes
                .intersects(LocalModes::ICANON | LocalModes::ECHO)
        );
        assert!(
            raw.local_modes.contains(LocalModes::ISIG),
            "Ctrl-C still exits"
        );
        assert_eq!(raw.special_codes[SpecialCodeIndex::VMIN], 1);
        assert_eq!(raw.special_codes[SpecialCodeIndex::VTIME], 0);

        let mut expected = before.clone();
        expected.local_modes = raw.local_modes;
        expected.special_codes[SpecialCodeIndex::VMIN] = 1;
        expected.special_codes[SpecialCodeIndex::VTIME] = 0;
        assert!(same(&raw, &expected), "{raw:?}\n{expected:?}");
        assert_eq!(
            raw.local_modes,
            before.local_modes - (LocalModes::ICANON | LocalModes::ECHO)
        );
    }

    #[test]
    fn dropping_raw_mode_restores_the_termios_it_found() {
        let (_controller, tty) = pty();
        let before = modes(&tty);
        drop(RawMode::enter(&tty).unwrap());
        assert!(same(&modes(&tty), &before));
    }

    #[test]
    fn a_panic_under_raw_mode_restores_the_termios_it_found() {
        let (_controller, tty) = pty();
        let before = modes(&tty);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _raw = RawMode::enter(&tty).unwrap();
            panic!("the view failed");
        }));
        assert!(unwound.is_err());
        assert!(same(&modes(&tty), &before));
    }

    /// What the signal handler runs, called directly: no signal is sent.
    #[test]
    fn the_handlers_restore_puts_back_the_saved_termios_and_the_screen() {
        let (controller, tty) = pty();
        let before = modes(&tty);
        let raw = RawMode::enter(&tty).unwrap();
        std::mem::forget(raw);
        assert!(!same(&modes(&tty), &before));

        restore_terminal(tty.as_fd(), Some(&before), tty.as_fd());
        assert!(same(&modes(&tty), &before));

        let mut written = Vec::new();
        while written.len() < RESTORE_SEQ.len() {
            let mut buf = [0u8; 64];
            let n = rustix::io::read(&controller, &mut buf).unwrap();
            assert!(n > 0, "the controller closed");
            written.extend_from_slice(&buf[..n]);
        }
        assert_eq!(written, RESTORE_SEQ.as_bytes());
    }

    #[test]
    fn with_nothing_saved_the_handlers_restore_still_restores_the_screen() {
        let (controller, tty) = pty();
        let before = modes(&tty);
        restore_terminal(tty.as_fd(), None, tty.as_fd());
        assert!(same(&modes(&tty), &before));
        let mut buf = [0u8; 64];
        let n = rustix::io::read(&controller, &mut buf).unwrap();
        assert_eq!(&buf[..n], RESTORE_SEQ.as_bytes());
    }
}
