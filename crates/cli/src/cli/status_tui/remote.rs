//! The remote switch: `clank web` as a process the TUI owns. Switched
//! on from the bar, it is started with its output piped so the line
//! it prints when listening becomes the URL the operator is shown and
//! sent to, and an exit before that line becomes the reason shown
//! instead; switched off, or dropped, it is ended by SIGTERM so its
//! own shutdown runs and its `zellij subscribe` child goes with it
//! (remote-is-a-switch-on-the-status-bar).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// What the bar draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Shown {
    #[default]
    Off,
    Starting,
    On,
    Failed,
}

/// What a process reported back: the server is listening; this
/// repo's server was already up (the launcher found it, said so and
/// left); a process — a server, or the launcher — is gone; the
/// browser could not be opened. An exit names its pid, so the
/// launcher's own exit after an `already on` is not the server's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    Listening {
        url: String,
    },
    AlreadyOn {
        url: String,
        pid: u32,
        attached_to: Option<u32>,
    },
    Exited {
        pid: u32,
        why: String,
    },
    BrowserFailed {
        why: String,
    },
}

/// A titled page for the operator, in the error overlay's shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Notice {
    pub(super) title: String,
    pub(super) message: String,
}

pub(super) type Report = Box<dyn FnMut(Outcome) + Send>;

/// The process operations, injected so the lifecycle — start, the
/// browser opened once, TERM on stop and on drop — is tested without
/// a binary (as `TabIndicator` injects its rename). Nothing here may
/// block the loop: results come back through the report.
pub(super) trait Io {
    /// Start `clank web` with `argv` (after the program name); its
    /// outcomes arrive through `report`. Returns the child's pid.
    fn start(&mut self, argv: Vec<String>, report: Report) -> Result<u32, String>;
    /// End the child: SIGTERM, a short wait, and only then SIGKILL.
    fn terminate(&mut self, pid: u32);
    /// Open `url` in the browser; a failure arrives through `report`.
    fn open(&mut self, url: &str, report: Report);
    /// Report `Exited` for `pid` once it is gone — for a server this
    /// switch adopted rather than started, whose exit no `wait` of
    /// ours will see.
    fn watch(&mut self, pid: u32, report: Report);
    fn alive(&self, pid: u32) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Off,
    Starting {
        pid: u32,
    },
    /// `owner` is the live process an already-running server follows
    /// that is not this TUI: shown, opened, never ended from here.
    On {
        pid: u32,
        url: String,
        owner: Option<u32>,
    },
    /// `why` stays readable on the row after the overlay is gone.
    Failed {
        why: String,
    },
}

pub(super) struct Remote<I: Io> {
    repo: PathBuf,
    io: I,
    state: State,
    /// Each start is a generation; an outcome from an earlier child —
    /// its exit arriving after the switch was thrown again — must not
    /// speak for the current one.
    generation: u64,
    outcomes: Arc<Mutex<Vec<(u64, Outcome)>>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl<I: Io> Remote<I> {
    /// `wake` is called from a process's reader thread whenever an
    /// outcome is queued, so the loop turns and drains it.
    pub(super) fn new(repo: PathBuf, io: I, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            repo,
            io,
            state: State::Off,
            generation: 0,
            outcomes: Arc::new(Mutex::new(Vec::new())),
            wake,
        }
    }

    pub(super) fn shown(&self) -> Shown {
        match self.state {
            State::Off => Shown::Off,
            State::Starting { .. } => Shown::Starting,
            State::On { .. } => Shown::On,
            State::Failed { .. } => Shown::Failed,
        }
    }

    /// What the row says beside the state: the URL when on, the
    /// reason when failed, nothing otherwise.
    pub(super) fn detail(&self) -> Option<String> {
        match &self.state {
            State::On {
                url,
                owner: Some(owner),
                ..
            } => Some(format!("{url}  pid {owner}'s")),
            State::On { url, .. } => Some(url.clone()),
            State::Failed { why } => Some(why.clone()),
            _ => None,
        }
    }

    /// Send the browser to the URL again; nothing unless on.
    pub(super) fn open_again(&mut self) {
        if let State::On { url, .. } = &self.state {
            let url = url.clone();
            let report = self.reporter();
            self.io.open(&url, report);
        }
    }

    /// A report for the current generation.
    fn reporter(&self) -> Report {
        let generation = self.generation;
        let outcomes = self.outcomes.clone();
        let wake = self.wake.clone();
        Box::new(move |o| {
            outcomes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((generation, o));
            wake();
        })
    }

    /// The switch: off or failed starts a server; starting or on ends
    /// it. A start that could not even begin is the one case with
    /// something to say right away.
    pub(super) fn toggle(&mut self) -> Option<Notice> {
        match &self.state {
            State::On {
                owner: Some(owner), ..
            } => Some(Notice {
                title: "remote".to_string(),
                message: format!("this server follows pid {owner}, not this TUI: stop it there"),
            }),
            State::Starting { pid } | State::On { pid, .. } => {
                let pid = *pid;
                self.io.terminate(pid);
                self.state = State::Off;
                None
            }
            State::Off | State::Failed { .. } => {
                self.generation += 1;
                let report = self.reporter();
                // No `--port`: the server binds the port the repo
                // remembers, or samples one, and reports a server
                // already up — the switch's whole adoption path runs
                // through that default (codex on fc1c941).
                let argv = vec![
                    "web".to_string(),
                    "--repo".to_string(),
                    self.repo.to_string_lossy().into_owned(),
                    "--attached-to".to_string(),
                    std::process::id().to_string(),
                ];
                match self.io.start(argv, report) {
                    Ok(pid) => {
                        self.state = State::Starting { pid };
                        None
                    }
                    Err(why) => {
                        self.state = State::Failed { why: why.clone() };
                        Some(Notice {
                            title: "remote".to_string(),
                            message: why,
                        })
                    }
                }
            }
        }
    }

    /// Absorb what the processes reported since the last turn: the
    /// listening line switches it on and sends the browser there —
    /// no notice, the row says the URL for as long as it is on; an
    /// exit switches it to failed with the reason; a browser that
    /// would not open is said, the server staying on. Outcomes of an
    /// earlier generation, or arriving once the switch is off, are
    /// nothing.
    pub(super) fn drain(&mut self) -> Vec<Notice> {
        let queued: Vec<(u64, Outcome)> =
            std::mem::take(&mut *self.outcomes.lock().unwrap_or_else(|e| e.into_inner()));
        let mut notices = Vec::new();
        for (generation, outcome) in queued {
            if generation != self.generation {
                continue;
            }
            let held = match &self.state {
                State::Starting { pid } | State::On { pid, .. } => Some(*pid),
                _ => None,
            };
            match (outcome, &self.state) {
                (Outcome::Listening { url }, State::Starting { pid }) => {
                    let pid = *pid;
                    let report = self.reporter();
                    self.io.open(&url, report);
                    self.state = State::On {
                        pid,
                        url,
                        owner: None,
                    };
                }
                // The launcher found this repo's server up. A server
                // following some other live process is theirs — shown,
                // never ended from here. Any other is adopted: owned,
                // and watched, since no `wait` of ours will see it go.
                (
                    Outcome::AlreadyOn {
                        url,
                        pid,
                        attached_to,
                    },
                    State::Starting { .. },
                ) => {
                    let me = std::process::id();
                    let owner = attached_to.filter(|m| *m != me && self.io.alive(*m));
                    if owner.is_none() {
                        let report = self.reporter();
                        self.io.watch(pid, report);
                    }
                    let report = self.reporter();
                    self.io.open(&url, report);
                    self.state = State::On { pid, url, owner };
                }
                (Outcome::Exited { pid, why }, _) if held == Some(pid) => {
                    notices.push(Notice {
                        title: "remote off".to_string(),
                        message: format!("clank web ended: {why}"),
                    });
                    self.state = State::Failed { why };
                }
                (Outcome::BrowserFailed { why }, State::On { url, .. }) => {
                    notices.push(Notice {
                        title: "remote on".to_string(),
                        message: format!("{url}\n\ncould not open a browser: {why}"),
                    });
                }
                _ => {}
            }
        }
        notices
    }
}

impl<I: Io> Drop for Remote<I> {
    fn drop(&mut self) {
        match &self.state {
            State::On { owner: Some(_), .. } => {}
            State::Starting { pid } | State::On { pid, .. } => self.io.terminate(*pid),
            _ => {}
        }
    }
}

/// `clank web: already on <url>  (pid <n>, attached to <m>|nobody)`,
/// as the launcher prints it.
fn parse_already_on(rest: &str) -> Option<Outcome> {
    let rest = rest.strip_prefix("already on ")?;
    let url = rest.split_whitespace().next()?.to_string();
    let inside = rest.split_once('(')?.1.strip_suffix(')')?;
    let (pid, attached) = inside.split_once(',')?;
    let pid: u32 = pid.trim().strip_prefix("pid ")?.trim().parse().ok()?;
    let attached = attached.trim().strip_prefix("attached to ")?.trim();
    let attached_to = match attached {
        "nobody" => None,
        m => Some(m.parse().ok()?),
    };
    Some(Outcome::AlreadyOn {
        url,
        pid,
        attached_to,
    })
}

/// How often an adopted server is looked for.
const WATCH_POLL: std::time::Duration = std::time::Duration::from_secs(2);

fn process_exists(pid: u32) -> bool {
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The real processes: a program run as `clank web`, SIGTERM, the
/// platform opener on a thread of its own.
pub(super) struct Processes {
    program: Program,
    /// Arguments before the ones a start supplies.
    prefix: Vec<String>,
}

enum Program {
    ThisBinary,
    #[cfg(test)]
    Path(PathBuf),
}

impl Processes {
    pub(super) fn clank() -> Self {
        Self {
            program: Program::ThisBinary,
            prefix: Vec::new(),
        }
    }

    /// `sh -c <script>` in the binary's place: the start's own
    /// arguments land as `$1..`, which the script ignores. Tests may
    /// not spawn clank; they may spawn a shell.
    #[cfg(test)]
    fn shell(script: &str) -> Self {
        Self {
            program: Program::Path(PathBuf::from("sh")),
            prefix: vec!["-c".to_string(), script.to_string(), "sh".to_string()],
        }
    }
}

/// How much of the child's stderr is kept for the reason it ended,
/// in whole lines, so no character is ever cut.
const STDERR_KEPT: usize = 4096;
/// How long the exit report waits for the stderr drain after the
/// child is gone. Its pipe closes with it — its own children get no
/// stderr of ours — so this is a bound, not an expectation.
const STDERR_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
const TERM_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Lines kept from the front-most out, within a byte budget.
fn keep_tail(lines: &mut std::collections::VecDeque<String>, line: String, budget: usize) {
    lines.push_back(line);
    let mut total: usize = lines.iter().map(|l| l.len() + 1).sum();
    while total > budget && lines.len() > 1 {
        if let Some(gone) = lines.pop_front() {
            total -= gone.len() + 1;
        }
    }
}

impl Io for Processes {
    fn start(&mut self, argv: Vec<String>, report: Report) -> Result<u32, String> {
        use std::io::BufRead;
        let program = match &self.program {
            Program::ThisBinary => {
                std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?
            }
            #[cfg(test)]
            Program::Path(p) => p.clone(),
        };
        let mut child = std::process::Command::new(program)
            .args(&self.prefix)
            .args(&argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start clank web: {e}"))?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        // Each pipe is drained on a thread of its own and the exit is
        // waited for on a third: a child's own children may hold a
        // pipe open past its death, and a chatty child would block on
        // a full one if the exit were waited for first. The exit
        // report then waits a bounded grace for the drained stderr,
        // so the reason is what the child said and not whatever had
        // arrived by the time it died (codex on 1764612).
        let (tail_tx, tail_rx) = std::sync::mpsc::channel::<String>();
        if let Some(stderr) = stderr {
            std::thread::spawn(move || {
                let mut lines = std::collections::VecDeque::new();
                for line in std::io::BufReader::new(stderr)
                    .lines()
                    .map_while(Result::ok)
                {
                    keep_tail(&mut lines, line, STDERR_KEPT);
                }
                let _ = tail_tx.send(lines.into_iter().collect::<Vec<_>>().join("\n"));
            });
        } else {
            drop(tail_tx);
        }
        let report = Arc::new(Mutex::new(report));
        // The exit report waits for the reader too: the launcher's
        // `already on` line must land before the launcher's exit, or
        // the exit would be taken for the server's.
        let (read_tx, read_rx) = std::sync::mpsc::channel::<()>();
        if let Some(stdout) = stdout {
            let report = report.clone();
            std::thread::spawn(move || {
                for line in std::io::BufReader::new(stdout)
                    .lines()
                    .map_while(Result::ok)
                {
                    let Some(rest) = line.strip_prefix("clank web: ") else {
                        continue;
                    };
                    let outcome = match parse_already_on(rest) {
                        Some(o) => Some(o),
                        None => rest
                            .split_whitespace()
                            .next()
                            .map(|url| Outcome::Listening {
                                url: url.to_string(),
                            }),
                    };
                    if let Some(o) = outcome {
                        (report.lock().unwrap_or_else(|e| e.into_inner()))(o);
                    }
                }
                let _ = read_tx.send(());
            });
        } else {
            drop(read_tx);
        }
        std::thread::spawn(move || {
            let status = child.wait();
            let _ = read_rx.recv_timeout(STDERR_GRACE);
            let stderr = tail_rx
                .recv_timeout(STDERR_GRACE)
                .map(|t| t.trim().to_string())
                .unwrap_or_default();
            let why = if stderr.is_empty() {
                match status {
                    Ok(s) => s.to_string(),
                    Err(e) => format!("could not wait for it: {e}"),
                }
            } else {
                stderr
            };
            (report.lock().unwrap_or_else(|e| e.into_inner()))(Outcome::Exited { pid, why });
        });
        Ok(pid)
    }

    fn watch(&mut self, pid: u32, mut report: Report) {
        std::thread::spawn(move || {
            while process_exists(pid) {
                std::thread::sleep(WATCH_POLL);
            }
            report(Outcome::Exited {
                pid,
                why: "the server is gone".to_string(),
            });
        });
    }

    fn alive(&self, pid: u32) -> bool {
        process_exists(pid)
    }

    fn terminate(&mut self, pid: u32) {
        let pid = pid as libc::pid_t;
        // Not `Child::kill`: that is SIGKILL, which runs none of the
        // server's shutdown and leaves its `zellij subscribe` child
        // streaming to nobody.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let deadline = std::time::Instant::now() + TERM_GRACE;
        while std::time::Instant::now() < deadline {
            if unsafe { libc::kill(pid, 0) } != 0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    fn open(&mut self, url: &str, mut report: Report) {
        // Off the loop, and with nothing on the terminal: the opener
        // may take its time or say something, and the TUI's raw
        // screen is not where either belongs (codex on 1764612).
        let url = url.to_string();
        std::thread::spawn(move || {
            let why = match crate::cli::html::opener() {
                Err(e) => Some(e.to_string()),
                Ok(mut cmd) => match cmd
                    .arg(&url)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .output()
                {
                    Err(e) => Some(format!("{e}")),
                    Ok(out) if out.status.success() => None,
                    Ok(out) => {
                        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                        Some(if err.is_empty() {
                            out.status.to_string()
                        } else {
                            err
                        })
                    }
                },
            };
            if let Some(why) = why {
                report(Outcome::BrowserFailed { why });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Log {
        started: Vec<Vec<String>>,
        terminated: Vec<u32>,
        opened: Vec<String>,
        reports: Vec<Report>,
        open_reports: Vec<Report>,
        watched: Vec<u32>,
        watch_reports: Vec<Report>,
        alive: Vec<u32>,
        refuse_start: Option<String>,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Log>>);

    impl Fake {
        fn log(&self) -> std::sync::MutexGuard<'_, Log> {
            self.0.lock().unwrap()
        }
        /// The child of the `n`th start speaks.
        fn child_says(&self, n: usize, outcome: Outcome) {
            let mut log = self.log();
            (log.reports[n])(outcome);
        }
        /// The `n`th opener fails.
        fn opener_fails(&self, n: usize, why: &str) {
            let mut log = self.log();
            (log.open_reports[n])(Outcome::BrowserFailed {
                why: why.to_string(),
            });
        }
        /// The `n`th watched pid is gone.
        fn watched_gone(&self, n: usize) {
            let mut log = self.log();
            let pid = log.watched[n];
            (log.watch_reports[n])(Outcome::Exited {
                pid,
                why: "the server is gone".into(),
            });
        }
    }

    impl Io for Fake {
        fn start(&mut self, argv: Vec<String>, report: Report) -> Result<u32, String> {
            let mut log = self.log();
            if let Some(why) = &log.refuse_start {
                return Err(why.clone());
            }
            log.started.push(argv);
            log.reports.push(report);
            Ok(1000 + log.started.len() as u32)
        }
        fn terminate(&mut self, pid: u32) {
            self.log().terminated.push(pid);
        }
        fn open(&mut self, url: &str, report: Report) {
            let mut log = self.log();
            log.opened.push(url.to_string());
            log.open_reports.push(report);
        }
        fn watch(&mut self, pid: u32, report: Report) {
            let mut log = self.log();
            log.watched.push(pid);
            log.watch_reports.push(report);
        }
        fn alive(&self, pid: u32) -> bool {
            self.log().alive.contains(&pid)
        }
    }

    fn remote(fake: &Fake) -> (Remote<Fake>, Arc<Mutex<usize>>) {
        let wakes = Arc::new(Mutex::new(0usize));
        let w = wakes.clone();
        let r = Remote::new(
            PathBuf::from("/r"),
            fake.clone(),
            Arc::new(move || *w.lock().unwrap() += 1),
        );
        (r, wakes)
    }

    fn listening() -> Outcome {
        Outcome::Listening {
            url: "http://127.0.0.1:8088".into(),
        }
    }

    /// Off → on: one start with this repo, port and pid, nothing shown
    /// until the child says it is listening, then the URL is shown and
    /// the browser sent there once; the wake is what turns the loop.
    #[test]
    fn the_switch_starts_one_server_and_opens_its_url_once() {
        let fake = Fake::default();
        let (mut r, wakes) = remote(&fake);
        assert_eq!(r.toggle(), None);
        assert_eq!(r.shown(), Shown::Starting);
        let argv = fake.log().started[0].clone();
        assert_eq!(
            argv,
            vec![
                "web",
                "--repo",
                "/r",
                "--attached-to",
                &std::process::id().to_string()
            ],
            "no --port: the repo's remembered port, and the already-on path, are the server's default"
        );
        assert!(!argv.iter().any(|a| a == "--port"));
        assert!(r.drain().is_empty(), "nothing to say before the child does");
        assert_eq!(r.detail(), None);
        fake.child_says(0, listening());
        assert_eq!(*wakes.lock().unwrap(), 1);
        assert!(
            r.drain().is_empty(),
            "listening is said by the row, not an overlay"
        );
        assert_eq!(r.shown(), Shown::On);
        assert_eq!(r.detail().as_deref(), Some("http://127.0.0.1:8088"));
        assert_eq!(fake.log().opened, vec!["http://127.0.0.1:8088"]);
        assert_eq!(fake.log().started.len(), 1);
        // `o` on the row: the browser goes there again; off, nothing.
        r.open_again();
        assert_eq!(fake.log().opened.len(), 2);
        r.toggle();
        r.open_again();
        assert_eq!(fake.log().opened.len(), 2);
    }

    /// A child that ends before it listens is a failure with its
    /// stderr as the reason, and no browser is sent anywhere.
    #[test]
    fn a_child_that_ends_first_is_shown_with_its_reason() {
        let fake = Fake::default();
        let (mut r, _) = remote(&fake);
        r.toggle();
        fake.child_says(
            0,
            Outcome::Exited {
                pid: 1001,
                why: "cannot listen on 127.0.0.1:8088: address in use".into(),
            },
        );
        let notices = r.drain();
        assert_eq!(notices[0].title, "remote off");
        assert!(notices[0].message.contains("address in use"), "{notices:?}");
        assert_eq!(r.shown(), Shown::Failed);
        assert_eq!(
            r.detail().as_deref(),
            Some("cannot listen on 127.0.0.1:8088: address in use"),
            "the reason stays on the row"
        );
        assert!(fake.log().opened.is_empty());
        // Failed → the switch starts again.
        r.toggle();
        assert_eq!(fake.log().started.len(), 2);
        assert_eq!(r.shown(), Shown::Starting);
    }

    /// A start that cannot begin says so at once.
    #[test]
    fn a_start_that_cannot_begin_is_shown_at_once() {
        let fake = Fake::default();
        fake.log().refuse_start = Some("current_exe failed".into());
        let (mut r, _) = remote(&fake);
        let notice = r.toggle().expect("a reason");
        assert!(notice.message.contains("current_exe failed"));
        assert_eq!(r.shown(), Shown::Failed);
        assert_eq!(r.detail().as_deref(), Some("current_exe failed"));
    }

    /// On → off ends the child by TERM; so does dropping the switch
    /// while it is on, or still starting. Nothing is ever terminated
    /// twice, and nothing is terminated when off.
    #[test]
    fn off_and_drop_end_the_child_by_term() {
        let fake = Fake::default();
        let (mut r, _) = remote(&fake);
        r.toggle();
        fake.child_says(0, listening());
        r.drain();
        assert_eq!(r.toggle(), None);
        assert_eq!(r.shown(), Shown::Off);
        assert_eq!(fake.log().terminated, vec![1001]);
        drop(r);
        assert_eq!(fake.log().terminated, vec![1001], "off has nothing to end");

        let (mut r, _) = remote(&fake);
        r.toggle();
        assert_eq!(r.shown(), Shown::Starting);
        drop(r);
        assert_eq!(fake.log().terminated, vec![1001, 1002]);
    }

    /// The ended child's own exit arrives after the switch was thrown
    /// off, or after it was thrown on again: neither speaks for the
    /// switch as it is now.
    #[test]
    fn a_stale_outcome_is_nothing() {
        let fake = Fake::default();
        let (mut r, _) = remote(&fake);
        r.toggle();
        r.toggle();
        fake.child_says(
            0,
            Outcome::Exited {
                pid: 1001,
                why: "terminated".into(),
            },
        );
        assert!(r.drain().is_empty());
        assert_eq!(r.shown(), Shown::Off);

        r.toggle();
        fake.child_says(
            0,
            Outcome::Listening {
                url: "http://old".into(),
            },
        );
        assert!(
            r.drain().is_empty(),
            "the first child's line is not the second's"
        );
        assert_eq!(r.shown(), Shown::Starting);
        fake.child_says(1, listening());
        assert!(r.drain().is_empty());
        assert_eq!(r.shown(), Shown::On);
    }

    /// A browser that will not open is said, later, and the server is
    /// still on; the opener's word for an earlier generation is not.
    #[test]
    fn an_unopenable_browser_is_said_and_the_server_stays_on() {
        let fake = Fake::default();
        let (mut r, _) = remote(&fake);
        r.toggle();
        fake.child_says(0, listening());
        assert!(r.drain().is_empty());
        fake.opener_fails(0, "no opener");
        let notices = r.drain();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].title, "remote on");
        assert!(
            notices[0]
                .message
                .contains("could not open a browser: no opener")
        );
        assert_eq!(r.shown(), Shown::On);

        r.toggle();
        r.toggle();
        fake.opener_fails(0, "late");
        assert!(r.drain().is_empty());
    }

    fn already_on(attached_to: Option<u32>) -> Outcome {
        Outcome::AlreadyOn {
            url: "http://127.0.0.1:53210".into(),
            pid: 777,
            attached_to,
        }
    }

    /// The launcher found this repo's server up and left: the switch
    /// is on with THAT pid, the URL shown and opened, the pid watched;
    /// the launcher's own exit is not the server's; the watcher
    /// saying the pid is gone is; Enter and drop end the adopted pid.
    #[test]
    fn a_server_already_up_is_adopted_watched_and_owned() {
        let fake = Fake::default();
        let (mut r, _) = remote(&fake);
        r.toggle();
        fake.child_says(0, already_on(None));
        fake.child_says(
            0,
            Outcome::Exited {
                pid: 1001,
                why: "exit status: 0".into(),
            },
        );
        assert!(r.drain().is_empty());
        assert_eq!(
            r.shown(),
            Shown::On,
            "the launcher's exit is not the server's"
        );
        assert_eq!(r.detail().as_deref(), Some("http://127.0.0.1:53210"));
        assert_eq!(fake.log().opened, vec!["http://127.0.0.1:53210"]);
        assert_eq!(fake.log().watched, vec![777]);

        fake.watched_gone(0);
        let notices = r.drain();
        assert_eq!(r.shown(), Shown::Failed);
        assert!(
            notices[0].message.contains("the server is gone"),
            "{notices:?}"
        );
        assert_eq!(r.detail().as_deref(), Some("the server is gone"));

        // Adopted again: Enter ends it, and so would leaving.
        r.toggle();
        fake.child_says(1, already_on(Some(std::process::id())));
        r.drain();
        assert_eq!(r.shown(), Shown::On, "attached to this TUI is ours");
        assert_eq!(r.toggle(), None);
        assert_eq!(fake.log().terminated, vec![777]);
        r.toggle();
        fake.child_says(2, already_on(None));
        r.drain();
        drop(r);
        assert_eq!(fake.log().terminated, vec![777, 777]);
    }

    /// A server following another LIVE process is somebody else's:
    /// shown with the owner, opened, not watched, and neither Enter
    /// nor leaving ends it. Attached to a dead pid, it is adopted.
    #[test]
    fn a_server_following_another_live_process_is_shown_not_owned() {
        let fake = Fake::default();
        fake.log().alive = vec![4242];
        let (mut r, _) = remote(&fake);
        r.toggle();
        fake.child_says(0, already_on(Some(4242)));
        assert!(r.drain().is_empty());
        assert_eq!(r.shown(), Shown::On);
        assert_eq!(
            r.detail().as_deref(),
            Some("http://127.0.0.1:53210  pid 4242's")
        );
        assert_eq!(fake.log().opened.len(), 1);
        assert!(fake.log().watched.is_empty(), "not ours to watch");
        let refused = r.toggle().expect("says whose it is");
        assert!(refused.message.contains("4242"), "{refused:?}");
        assert_eq!(r.shown(), Shown::On);
        drop(r);
        assert!(fake.log().terminated.is_empty(), "never ended from here");

        let (mut r, _) = remote(&fake);
        r.toggle();
        fake.child_says(1, already_on(Some(9999)));
        r.drain();
        assert_eq!(fake.log().watched, vec![777], "a dead owner: adopted");
    }

    /// The launcher's line, as the server prints it.
    #[test]
    fn the_already_on_line_is_read_whole() {
        assert_eq!(
            parse_already_on("already on http://127.0.0.1:53210  (pid 777, attached to nobody)"),
            Some(already_on(None))
        );
        assert_eq!(
            parse_already_on("already on http://127.0.0.1:53210  (pid 777, attached to 4242)"),
            Some(already_on(Some(4242)))
        );
        assert_eq!(
            parse_already_on("http://127.0.0.1:53210  (session s)"),
            None
        );
        assert_eq!(parse_already_on("already on http://x (pid seven)"), None);
    }

    fn outcomes_of(processes: &mut Processes) -> (u32, std::sync::mpsc::Receiver<Outcome>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let pid = processes
            .start(
                vec!["web".into(), "--port".into(), "1".into()],
                Box::new(move |o| {
                    let _ = tx.send(o);
                }),
            )
            .unwrap();
        (pid, rx)
    }

    const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

    /// The real reader: the listening line is the URL, and a
    /// terminated child's exit follows it. `sh`, not this binary.
    #[test]
    fn the_listening_line_is_the_url_and_the_exit_follows_a_term() {
        let mut p = Processes::shell(
            "echo 'clank web: http://127.0.0.1:1  (session s, 2 agent panes)'; exec sleep 30",
        );
        let (pid, rx) = outcomes_of(&mut p);
        assert_eq!(
            rx.recv_timeout(WAIT).unwrap(),
            Outcome::Listening {
                url: "http://127.0.0.1:1".into()
            }
        );
        p.terminate(pid);
        assert!(matches!(
            rx.recv_timeout(WAIT).unwrap(),
            Outcome::Exited { pid: p, .. } if p == pid
        ));
    }

    /// The launcher's line through the real reader, and its own exit
    /// after it — in that order, whatever the threads' timing.
    #[test]
    fn an_already_on_line_lands_before_the_launchers_exit() {
        for _ in 0..5 {
            let mut p = Processes::shell(
                "echo 'clank web: already on http://127.0.0.1:53210  (pid 777, attached to nobody)'",
            );
            let (pid, rx) = outcomes_of(&mut p);
            assert_eq!(rx.recv_timeout(WAIT).unwrap(), already_on(None));
            assert!(matches!(
                rx.recv_timeout(WAIT).unwrap(),
                Outcome::Exited { pid: p, .. } if p == pid
            ));
        }
    }

    /// A child whose own child outlives it, holding its pipes, is
    /// still reported gone within the grace — the exit is waited for,
    /// not inferred from the pipes closing.
    #[test]
    fn a_pipe_held_by_a_grandchild_does_not_delay_the_exit() {
        let mut p = Processes::shell("(sleep 20 &); echo 'held' >&2; exit 2");
        let started = std::time::Instant::now();
        let (_, rx) = outcomes_of(&mut p);
        match rx.recv_timeout(WAIT).unwrap() {
            Outcome::Exited { why, .. } => assert!(why == "held" || why.contains('2'), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(
            started.elapsed() < WAIT,
            "within the grace, not the grandchild's life"
        );
    }

    /// A child that ends with something on stderr ends with THAT as
    /// its reason — the drain is waited for, not sampled — and one
    /// that says nothing ends with its status.
    #[test]
    fn a_childs_last_words_are_its_reason() {
        let mut p = Processes::shell("echo 'cannot listen: address in use' >&2; exit 1");
        let (pid, rx) = outcomes_of(&mut p);
        assert_eq!(
            rx.recv_timeout(WAIT).unwrap(),
            Outcome::Exited {
                pid,
                why: "cannot listen: address in use".into()
            }
        );
        let mut p = Processes::shell("exit 3");
        let (_, rx) = outcomes_of(&mut p);
        match rx.recv_timeout(WAIT).unwrap() {
            Outcome::Exited { why, .. } => assert!(why.contains('3'), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    /// A long, multi-byte stderr is kept as its last whole lines
    /// within the budget: nothing is cut inside a character, and the
    /// last line is there in full.
    #[test]
    fn a_long_unicode_stderr_is_kept_whole() {
        let mut p = Processes::shell(
            "i=0; while [ $i -lt 300 ]; do echo \"ligne $i ééééééééééééééééééééééééééééééé\" >&2; i=$((i+1)); done; exit 1",
        );
        let (_, rx) = outcomes_of(&mut p);
        match rx.recv_timeout(WAIT).unwrap() {
            Outcome::Exited { why, .. } => {
                assert!(
                    why.ends_with("ligne 299 ééééééééééééééééééééééééééééééé"),
                    "{why}"
                );
                assert!(why.len() <= STDERR_KEPT, "{}", why.len());
                assert!(why.starts_with("ligne "), "whole lines only: {why:?}");
            }
            other => panic!("{other:?}"),
        }
        let mut lines = std::collections::VecDeque::new();
        keep_tail(&mut lines, "é".repeat(10), 8);
        assert_eq!(lines.len(), 1, "one line over budget is still kept whole");
    }

    /// The real terminate is TERM first: a child that dies of it
    /// reports signal 15, and does so within the grace, not after it.
    #[test]
    fn terminate_sends_term_first() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep");
        let pid = child.id();
        // Reaped on its own thread, so terminate sees it gone rather
        // than a zombie that outlasts the grace.
        let reaper = std::thread::spawn(move || child.wait());
        let started = std::time::Instant::now();
        Processes::clank().terminate(pid);
        let status = reaper.join().unwrap().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM), "{status:?}");
        assert!(
            started.elapsed() < TERM_GRACE,
            "gone before the grace ran out"
        );
    }
}
