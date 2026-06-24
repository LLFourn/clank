//! A console [`Screen`]: one child process in a PTY plus the
//! `vt100` grid that mirrors what it has drawn.
//!
//! A dedicated reader thread ALWAYS drains the PTY into the grid —
//! even when the screen is backgrounded — because a child whose
//! ~64KB PTY buffer fills blocks on its next write and hangs. The
//! drain (in the reader) and the repaint (in the loop) are SEPARATE
//! concerns: the reader never stops reading; it only signals a
//! repaint *opportunity*, which the loop coalesces and ignores
//! unless this screen is active.

use std::os::unix::io::RawFd;
use std::path::Path;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use super::pty;

/// What to run in a screen: a chrome label plus the argv. The
/// console builds these from the roster (`clank agent start <label>`
/// per agent, plus a `clank status --tui` screen). Cloned onto the
/// `Screen` so a dead one can be manually respawned in place.
#[derive(Clone)]
pub(crate) struct Spec {
    pub(crate) label: String,
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
}

/// A reader-thread signal. `Output` carries the screen index so the
/// loop can repaint iff that screen is active; `Exit` (the child's
/// PTY hit EOF) is payload-free — the loop re-scans the shared
/// `alive` flags, which are the source of truth for liveness.
pub(crate) enum Event {
    Output(usize),
    Exit,
}

pub(crate) struct Screen {
    pub(crate) label: String,
    pub(crate) master: RawFd,
    pub(crate) child: Child,
    pub(crate) parser: Arc<Mutex<vt100::Parser>>,
    pub(crate) alive: Arc<AtomicBool>,
    /// What this screen runs, kept so a dead screen can be respawned.
    pub(crate) spec: Spec,
}

impl Screen {
    /// Spawn the child in a PTY sized to the content region and start
    /// its always-on reader thread. `idx` tags the events this
    /// screen emits so the loop knows which grid changed.
    pub(crate) fn spawn(
        spec: &Spec,
        cwd: &Path,
        rows: u16,
        cols: u16,
        idx: usize,
        tx: Sender<Event>,
    ) -> std::io::Result<Self> {
        let (master, child) = pty::spawn(&spec.program, &spec.args, cwd, rows, cols)?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let alive = Arc::new(AtomicBool::new(true));

        let reader_parser = parser.clone();
        let reader_alive = alive.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                // SAFETY: `master` is a live fd this thread owns the
                // read end of; `buf` is a valid local buffer.
                let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    reader_alive.store(false, Ordering::Relaxed);
                    let _ = tx.send(Event::Exit);
                    return;
                }
                // Drain ALWAYS — feed the grid regardless of focus.
                if let Ok(mut p) = reader_parser.lock() {
                    p.process(&buf[..n as usize]);
                }
                // Signal only; the loop repaints iff this is active.
                if tx.send(Event::Output(idx)).is_err() {
                    return;
                }
            }
        });

        Ok(Screen {
            label: spec.label.clone(),
            master,
            child,
            parser,
            alive,
            spec: spec.clone(),
        })
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Spawn a trivial NON-clank child (`printf`) through the real
    /// PTY path and assert the emulator grid reflects what it wrote.
    /// This exercises pty.rs + the reader thread + vt100 wiring end
    /// to end without spawning clank or zellij (the no-binary-spawn
    /// ban is about the *clank* binary; a stock `printf` is fine).
    #[test]
    fn child_output_lands_in_the_grid() {
        let (tx, rx) = mpsc::channel();
        let spec = Spec {
            label: "printf".into(),
            program: "printf".into(),
            args: vec!["hello console".into()],
        };
        let screen =
            Screen::spawn(&spec, Path::new("/"), 24, 80, 0, tx).expect("spawn printf in a pty");

        // Wait (bounded) for the child to write + exit.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Event::Exit) => break,
                Ok(Event::Output(_)) => {}
                Err(_) if Instant::now() > deadline => panic!("child never exited"),
                Err(_) => {}
            }
        }

        let parser = screen.parser.lock().unwrap();
        let row0 = parser
            .screen()
            .contents()
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        assert!(row0.starts_with("hello console"), "grid row 0 was {row0:?}");
    }

    #[test]
    fn reader_reports_exit_and_clears_alive() {
        let (tx, rx) = mpsc::channel();
        let spec = Spec {
            label: "true".into(),
            program: "true".into(),
            args: vec![],
        };
        let screen =
            Screen::spawn(&spec, Path::new("/"), 24, 80, 7, tx).expect("spawn `true` in a pty");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_exit = false;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Event::Exit) => {
                    saw_exit = true;
                    break;
                }
                Ok(Event::Output(_)) => {}
                Err(_) => {}
            }
        }
        assert!(saw_exit, "reader must emit Exit on EOF");
        assert!(!screen.is_alive(), "alive flag cleared on EOF");
    }
}
