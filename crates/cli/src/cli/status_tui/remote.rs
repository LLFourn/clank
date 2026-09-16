//! The remote switch: the repo's page, served from inside the TUI as
//! one owned [`Instance`]. Switched on from the row, it binds the
//! port the repo remembers and the browser is sent there; switched
//! off, or the TUI closed, everything it started is ended and joined
//! (clank-tui-runs-the-remote-in-process).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::cli::web::door::{Door, Link};
use crate::cli::web::{Instance, Panes};

/// What the tunnel source last said, as the row draws it.
enum Configured {
    /// No tunnel in the config: the switch refuses.
    Absent,
    /// A tunnel the source could build.
    Ready,
    /// A tunnel IS configured but could not be built — a retired
    /// provider, a config that will not parse. The reason is the
    /// operator's to see; "not configured" is not it.
    Broken(String),
}

/// What the row says beside `remote` when no tunnel is configured.
const UNCONFIGURED: &str = "not configured";

/// And what the switch says when it is thrown anyway.
const UNCONFIGURED_HOW: &str = "No remote is configured, so there is nowhere to serve it.\n\n\
     Add a tunnel to ~/.clank/config.json, for example:\n\n\
     \"remote\": { \"tunnel\": { \"provider\": \"quick\" } }\n\n\
     `quick` needs no account, no domain and nothing installed; \
     `command` runs any binary that forwards the port.";

/// What the bar and the row draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Shown {
    #[default]
    Off,
    Starting,
    On,
    Stopping,
    Failed,
    /// No tunnel configured. The remote exists to be reached from
    /// elsewhere, and a page bound to loopback reaches nobody, so
    /// this is a state rather than a start that quietly serves
    /// nothing useful (the-tunnel-says-its-own-name).
    Unconfigured,
}

/// What a task or thread of the switch's reported back: the start it
/// was asked for came up, or did not; the stop it was asked for is
/// done; the browser could not be opened.
pub(super) enum Outcome {
    /// Boxed: an instance is the whole remote, and the other
    /// outcomes are a sentence.
    Started(Box<Instance>),
    StartFailed {
        why: String,
    },
    Stopped,
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

/// How the browser is sent to a URL, injected so the switch is tested
/// without opening one. Nothing here may block the loop: a failure
/// comes back through the report.
pub(super) trait Opener: Send + 'static {
    fn open(&mut self, url: &str, report: Report);
}

/// Where the zellij side comes from when the switch is thrown: found
/// then, not at the TUI's start, so a TUI outside a session still
/// runs and the row says why the remote cannot.
pub(super) type PanesSource = Arc<dyn Fn() -> anyhow::Result<Arc<dyn Panes>> + Send + Sync>;

/// Where the tunnel provider comes from when the switch is thrown:
/// the user config as it is then, so a tunnel configured after the
/// TUI started is the next start's.
pub(super) type TunnelSource = Arc<
    dyn Fn() -> anyhow::Result<
            Option<(
                Arc<dyn crate::cli::web::tunnel::Provider>,
                std::time::Duration,
            )>,
        > + Send
        + Sync,
>;

/// Starting and stopping are tasks in flight, held so the TUI's exit
/// can wait for them; the loop is never inside either (codex on
/// 55db154).
enum State {
    Off,
    Starting {
        task: tokio::task::JoinHandle<()>,
        /// Thrown to abandon a tunnel still coming up: the exit path
        /// does not wait out a connect that has stalled.
        abort: tokio::sync::watch::Sender<bool>,
    },
    On {
        instance: Box<Instance>,
    },
    Stopping {
        task: tokio::task::JoinHandle<()>,
    },
    /// `why` stays readable on the row after the overlay is gone.
    Failed {
        why: String,
    },
}

pub(super) struct Remote<O: Opener> {
    repo: PathBuf,
    home: Option<PathBuf>,
    panes: PanesSource,
    /// The door outlives any one instance: its token and sessions
    /// are the user's, shown and revoked from the row whether the
    /// remote is on or off.
    door: Arc<Door>,
    tunnel: TunnelSource,
    /// What the tunnel source last said. For DRAWING only — the row
    /// is painted far more often than the config changes, so it
    /// reads a remembered answer — and never for deciding whether a
    /// start is legal, which only the fresh read in the start task
    /// may decide (codex on 5149876).
    configured: std::cell::RefCell<Configured>,
    /// How often the panes are re-listed, and how long a stop lets
    /// the tasks end on their own; the production values, or a test's.
    poll: std::time::Duration,
    grace: std::time::Duration,
    opener: O,
    state: State,
    /// Each start is a generation; an opener's word about an earlier
    /// one must not speak for the current one.
    generation: u64,
    outcomes: Arc<Mutex<Vec<(u64, Outcome)>>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl<O: Opener> Remote<O> {
    /// `wake` is called from a thread whenever an outcome is queued,
    /// so the loop turns and drains it.
    pub(super) fn new(
        repo: PathBuf,
        home: Option<PathBuf>,
        panes: PanesSource,
        door: Arc<Door>,
        poll: std::time::Duration,
        grace: std::time::Duration,
        opener: O,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            repo,
            home,
            panes,
            door,
            tunnel: Arc::new(|| Ok(None)),
            configured: std::cell::RefCell::new(Configured::Absent),
            poll,
            grace,
            opener,
            state: State::Off,
            generation: 0,
            outcomes: Arc::new(Mutex::new(Vec::new())),
            wake,
        }
    }

    /// Where the tunnel provider is read from at each start.
    pub(super) fn tunnel_from(mut self, source: TunnelSource) -> Self {
        self.tunnel = source;
        self.recheck_configured();
        self
    }

    /// Re-read what the tunnel source says. Cheap enough for a
    /// snapshot rebuild and far too dear for a frame, which is why
    /// the row reads a remembered answer. An error is KEPT as an
    /// error: a provider that is configured but cannot be built —
    /// a retired one, a config that will not parse — must not be
    /// reported as no tunnel at all (codex on 5149876).
    fn recheck_configured(&self) {
        *self.configured.borrow_mut() = match (self.tunnel)() {
            Ok(Some(_)) => Configured::Ready,
            Ok(None) => Configured::Absent,
            Err(e) => Configured::Broken(format!("{e:#}")),
        };
    }

    pub(super) fn shown(&self) -> Shown {
        match self.state {
            State::Off => match &*self.configured.borrow() {
                Configured::Absent => Shown::Unconfigured,
                Configured::Broken(_) => Shown::Failed,
                Configured::Ready => Shown::Off,
            },
            State::Starting { .. } => Shown::Starting,
            State::On { .. } => Shown::On,
            State::Stopping { .. } => Shown::Stopping,
            State::Failed { .. } => Shown::Failed,
        }
    }

    /// What the row says beside the state: the URL when on, the
    /// reason when failed, nothing otherwise.
    pub(super) fn detail(&self) -> Option<String> {
        match &self.state {
            State::On { instance } => Some(
                instance
                    .public_url
                    .clone()
                    .unwrap_or_else(|| instance.url.clone()),
            ),
            State::Failed { why } => Some(why.clone()),
            State::Off => match &*self.configured.borrow() {
                Configured::Absent => Some(UNCONFIGURED.to_string()),
                Configured::Broken(why) => Some(why.clone()),
                Configured::Ready => None,
            },
            _ => None,
        }
    }

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

    /// The switch. Off or failed: a start begins, as a task, and the
    /// row reads `starting…` until its `Started` arrives; on: a stop
    /// begins, as a task, and the row reads `stopping…` until its
    /// `Stopped`. Neither is done on the loop, so a slow zellij or a
    /// long join never freezes the pane. A press while either is in
    /// flight is nothing.
    pub(super) fn toggle(&mut self) -> Option<Notice> {
        match std::mem::replace(&mut self.state, State::Off) {
            State::On { instance } => {
                let mut report = self.reporter();
                let task = tokio::spawn(async move {
                    instance.stop().await;
                    report(Outcome::Stopped);
                });
                self.state = State::Stopping { task };
                None
            }
            s @ (State::Starting { .. } | State::Stopping { .. }) => {
                self.state = s;
                None
            }
            State::Off | State::Failed { .. } => {
                // A fast, fresh refusal so the common "never set it
                // up" case answers on the keypress. It is NOT the
                // gate: the start task reads again, because the
                // config can change while zellij is being found.
                self.recheck_configured();
                if matches!(&*self.configured.borrow(), Configured::Absent) {
                    self.state = State::Off;
                    return Some(Notice {
                        title: "remote".to_string(),
                        message: UNCONFIGURED_HOW.to_string(),
                    });
                }
                self.generation += 1;
                let mut report = self.reporter();
                let (repo, home, poll, grace) =
                    (self.repo.clone(), self.home.clone(), self.poll, self.grace);
                // Finding zellij is the task's too: it lists the session
                // synchronously, which is not the loop's to wait for
                // (codex on 369e74e).
                let panes = self.panes.clone();
                let door = self.door.clone();
                let tunnel = self.tunnel.clone();
                let (abort, abandoned) = tokio::sync::watch::channel(false);
                let task = tokio::spawn(async move {
                    let found = tokio::task::spawn_blocking(move || Ok((panes()?, tunnel()?)))
                        .await
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("finding zellij panicked")));
                    let started = match found {
                        // THE gate: the tunnel read fresh, beside the
                        // panes, in the same task that starts. A
                        // remote with none is not started at all.
                        Ok((panes, Some((provider, tunnel_grace)))) => {
                            let start = crate::cli::web::tunnel::Start {
                                provider,
                                grace: tunnel_grace,
                                abort: abandoned,
                                ask: std::sync::Arc::new(crate::cli::web::dns::Net::default()),
                            };
                            Instance::start(repo, home, panes, door, Some(start), poll, grace).await
                        }
                        Ok((_, None)) => Err(anyhow::anyhow!("{UNCONFIGURED_HOW}")),
                        Err(e) => Err(e),
                    };
                    report(match started {
                        Ok(instance) => Outcome::Started(Box::new(instance)),
                        Err(e) => Outcome::StartFailed {
                            why: format!("{e:#}"),
                        },
                    });
                });
                self.state = State::Starting { task, abort };
                None
            }
        }
    }

    /// The TUI rebuilt its snapshot; the page follows.
    pub(super) fn observe(&self, snap: &crate::cli::status::StatusSnapshot) {
        self.recheck_configured();
        if let State::On { instance } = &self.state {
            instance.observe(snap);
        }
    }

    /// Send the browser in again — through a fresh login link, so
    /// the desktop needs no ceremony; nothing unless on.
    pub(super) fn open_again(&mut self) {
        if let Some(url) = self.link() {
            let report = self.reporter();
            self.opener.open(&url, report);
        }
    }

    /// A one-time link into the running remote: the tunnel's proven
    /// URL when there is one, else the local one.
    ///
    /// ONE link, not one per destination. The row's `detail` already
    /// preferred the public URL while the browser was sent to
    /// loopback, and a second place to express the preference is
    /// exactly how the two came to disagree. The instance's URL, not
    /// the config's: a domain changed while the remote is on belongs
    /// to the next start (codex on 6f60efa).
    pub(super) fn link(&self) -> Option<String> {
        match &self.state {
            State::On { instance } => {
                let base = instance
                    .public_url
                    .clone()
                    .unwrap_or_else(|| instance.url.clone());
                Some(format!(
                    "{}/login?t={}",
                    base.trim_end_matches('/'),
                    self.door.mint(Link::Login)
                ))
            }
            _ => None,
        }
    }

    pub(super) fn door(&self) -> &Arc<Door> {
        &self.door
    }

    /// End the remote, everything joined: the TUI's exit path, where
    /// waiting is right. A start or stop in flight is waited for too,
    /// and a start that lands is stopped in turn.
    pub(super) async fn stop(&mut self) {
        match std::mem::replace(&mut self.state, State::Off) {
            State::On { instance } => instance.stop().await,
            State::Starting { task, abort } => {
                let _ = abort.send(true);
                let _ = task.await;
                self.stop_landed().await;
            }
            State::Stopping { task } => {
                let _ = task.await;
                self.stop_landed().await;
            }
            State::Off | State::Failed { .. } => {}
        }
    }

    /// Stop an instance a start reported while the exit was under
    /// way: nothing else will. The queue is taken before the await,
    /// not held across it.
    async fn stop_landed(&mut self) {
        let queued: Vec<(u64, Outcome)> =
            std::mem::take(&mut *self.outcomes.lock().unwrap_or_else(|e| e.into_inner()));
        for (_, outcome) in queued {
            if let Outcome::Started(instance) = outcome {
                instance.stop().await;
            }
        }
    }

    /// Absorb what the tasks and threads reported since the last
    /// turn. A start that landed is on, seeded with the loop's current
    /// snapshot and the browser sent to it; one that failed says why;
    /// a stop that finished is off. Outcomes of an earlier generation
    /// are nothing — except an instance, which is stopped, since
    /// nothing else will.
    pub(super) fn drain(&mut self, snap: &crate::cli::status::StatusSnapshot) -> Vec<Notice> {
        let queued: Vec<(u64, Outcome)> =
            std::mem::take(&mut *self.outcomes.lock().unwrap_or_else(|e| e.into_inner()));
        let mut notices = Vec::new();
        for (generation, outcome) in queued {
            if generation != self.generation {
                if let Outcome::Started(instance) = outcome {
                    tokio::spawn(instance.stop());
                }
                continue;
            }
            match (outcome, &self.state) {
                (Outcome::Started(instance), State::Starting { .. }) => {
                    instance.observe(snap);
                    self.state = State::On { instance };
                    self.open_again();
                }
                (Outcome::Started(instance), _) => {
                    tokio::spawn(instance.stop());
                }
                (Outcome::StartFailed { why }, State::Starting { .. }) => {
                    self.state = State::Failed { why: why.clone() };
                    notices.push(Notice {
                        title: "remote".to_string(),
                        message: why,
                    });
                }
                (Outcome::Stopped, State::Stopping { .. }) => self.state = State::Off,
                (Outcome::BrowserFailed { why }, State::On { instance }) => {
                    notices.push(Notice {
                        title: "remote on".to_string(),
                        // The URL it actually tried, not loopback: a
                        // failure that names a different address than
                        // the one it used sends the user nowhere.
                        message: format!(
                            "{}\n\ncould not open a browser: {why}",
                            instance
                                .public_url
                                .clone()
                                .unwrap_or_else(|| instance.url.clone())
                        ),
                    });
                }
                _ => {}
            }
        }
        notices
    }
}

/// A QR of `text` as lines of half-block cells, two module rows per
/// line, a quiet zone around: light modules are the block, dark ones
/// the gap, so drawn white-on-black the code reads dark-on-light as
/// a camera expects. Empty if the text is too long to encode.
pub(super) fn qr_lines(text: &str) -> Vec<String> {
    let Ok(code) = qrcode::QrCode::new(text.as_bytes()) else {
        return Vec::new();
    };
    let width = code.width();
    let colors = code.to_colors();
    const QUIET: usize = 4;
    let side = width + 2 * QUIET;
    let dark = |x: usize, y: usize| {
        (QUIET..QUIET + width).contains(&x)
            && (QUIET..QUIET + width).contains(&y)
            && colors[(y - QUIET) * width + (x - QUIET)] == qrcode::Color::Dark
    };
    (0..side)
        .step_by(2)
        .map(|y| {
            (0..side)
                .map(|x| match (dark(x, y), dark(x, y + 1)) {
                    (false, false) => '█',
                    (false, true) => '▀',
                    (true, false) => '▄',
                    (true, true) => ' ',
                })
                .collect()
        })
        .collect()
}

/// The platform opener on a thread of its own: off the loop, and with
/// nothing on the terminal — the opener may take its time or say
/// something, and the TUI's raw screen is not where either belongs.
pub(super) struct Browser;

impl Opener for Browser {
    fn open(&mut self, url: &str, mut report: Report) {
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

    /// The browser is sent where the row says. ONE link, so the two
    /// cannot drift: the bug was a `detail` that preferred the tunnel
    /// while the opener preferred loopback.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_browser_is_opened_at_the_url_the_row_shows() {
        let repo = repo_with_config();
        let (mut r, rec) = remote(repo.path(), true);
        let snap = snap();
        r.toggle();
        settle(&mut r, &snap).await;
        assert_eq!(r.shown(), Shown::On);

        let shown = r.detail().expect("on, so the row shows a URL");
        let link = r.link().expect("on, so there is a link");
        assert!(
            link.starts_with(&format!("{shown}/login?t=")),
            "the link is built on the URL the row shows: {link} vs {shown}"
        );

        let opened = rec.opened.lock().unwrap().clone();
        assert_eq!(opened.len(), 1, "opened once on coming on");
        assert!(
            opened[0].starts_with(&format!("{shown}/login?t=")),
            "the browser was sent to the row's URL: {:?} vs {shown}",
            opened[0]
        );
        r.stop().await;
    }
    use super::*;
    use crate::cli::open_zellij::{PaneMeta, SubscribeChild};

    /// Two module rows per line, a four-module quiet zone of light
    /// cells, and the finder pattern where every QR has one: the
    /// first module row is seven dark, the second dark-light-dark.
    #[test]
    fn a_qr_is_half_block_lines_with_a_quiet_zone() {
        let lines = qr_lines("http://localhost:1234/register?t=abc");
        let side = lines[0].chars().count();
        assert!(side >= 21 + 8);
        assert_eq!(lines.len(), side.div_ceil(2));
        assert!(
            lines[0].chars().all(|c| c == '█'),
            "quiet zone: {}",
            lines[0]
        );
        let third: Vec<char> = lines[2].chars().collect();
        assert_eq!(&third[..4], ['█'; 4], "{}", lines[2]);
        assert_eq!(third[4], ' ', "dark over dark: {}", lines[2]);
        assert_eq!(third[5], '▄', "dark over light: {}", lines[2]);
        assert_eq!(third[10], ' ', "{}", lines[2]);
        assert_eq!(third[11], '█', "{}", lines[2]);
        assert!(qr_lines(&"x".repeat(5000)).is_empty(), "too long to encode");
    }

    /// A zellij with no agent panes and nothing to subscribe to, or
    /// one that does not answer.
    struct FakePanes {
        answers: bool,
        /// A listing that takes its time, as zellij under load does.
        slow: std::time::Duration,
    }

    impl Panes for FakePanes {
        fn session(&self) -> &str {
            "s"
        }
        fn table(&self) -> Option<Vec<PaneMeta>> {
            std::thread::sleep(self.slow);
            self.answers.then(Vec::new)
        }
        fn subscribe(&self, _ids: &[String]) -> Option<SubscribeChild> {
            None
        }
        fn say(&self, _pane: &str, _text: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct Recorder {
        opened: Arc<Mutex<Vec<String>>>,
        reports: Arc<Mutex<Vec<Report>>>,
    }

    impl Opener for Recorder {
        fn open(&mut self, url: &str, report: Report) {
            self.opened.lock().unwrap().push(url.to_string());
            self.reports.lock().unwrap().push(report);
        }
    }

    fn repo_with_config() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        crate::agent_store::write_typed_config(
            &dir.path().join(".clank/config.json"),
            &crate::cli::teams_config::RepoConfigFile::default(),
        )
        .unwrap();
        dir
    }

    fn remote(repo: &std::path::Path, answers: bool) -> (Remote<Recorder>, Recorder) {
        remote_with(
            repo,
            answers,
            std::time::Duration::ZERO,
            crate::cli::web::PANE_POLL,
            crate::cli::web::STOP_GRACE,
        )
    }

    /// `slow` is both how long finding zellij takes and how long each
    /// listing does.
    /// A remote with a tunnel that forwards to the instance's own
    /// loopback URL. Every remote has one now — a remote with no
    /// tunnel refuses to start at all — so the lifecycle tests carry
    /// the smallest tunnel that is still a tunnel.
    fn remote_with(
        repo: &std::path::Path,
        answers: bool,
        slow: std::time::Duration,
        poll: std::time::Duration,
        grace: std::time::Duration,
    ) -> (Remote<Recorder>, Recorder) {
        // A tunnel needs a home for its lease, and this fake one
        // needs the port before the instance binds it, so the port
        // is settled up front — but ONLY when zellij will answer,
        // because a start that fails before listening must leave the
        // repo with no port remembered.
        let home = repo.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let port = if answers {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            crate::agent_store::record_web_port(repo, p).unwrap();
            p
        } else {
            0
        };
        let rec = Recorder::default();
        let provider = Arc::new(FakeTunnel {
            allocated: false,
            // The loopback LITERAL, while the instance reports itself
            // as `localhost`. Both reach the same server, so the probe
            // passes, but they differ as strings — which is the only
            // way a test can tell the public URL from the local one.
            url: format!("http://127.0.0.1:{port}"),
            pending: false,
            started: Default::default(),
            stopped: Default::default(),
        });
        let r = Remote::new(
            repo.to_path_buf(),
            Some(home),
            Arc::new(move || {
                std::thread::sleep(slow);
                Ok(Arc::new(FakePanes { answers, slow }) as Arc<dyn Panes>)
            }),
            Arc::new(Door::new(None)),
            poll,
            grace,
            rec.clone(),
            Arc::new(|| {}),
        )
        .tunnel_from(Arc::new(move || {
            Ok(Some((
                provider.clone() as Arc<dyn crate::cli::web::tunnel::Provider>,
                std::time::Duration::from_secs(5),
            )))
        }));
        (r, rec)
    }

    /// The display cache is not the gate. A source that said Some
    /// when the row was painted and says None by the time the start
    /// task reads it must NOT bind a loopback-only server: the
    /// fresh read in the task decides (codex on 5149876).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tunnel_removed_after_the_row_was_painted_still_refuses() {
        let repo = repo_with_config();
        let home = repo.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counting = reads.clone();
        let rec = Recorder::default();
        let mut r = Remote::new(
            repo.path().to_path_buf(),
            Some(home),
            Arc::new(|| {
                Ok(Arc::new(FakePanes {
                    answers: true,
                    slow: std::time::Duration::ZERO,
                }) as Arc<dyn Panes>)
            }),
            Arc::new(Door::new(None)),
            crate::cli::web::PANE_POLL,
            crate::cli::web::STOP_GRACE,
            rec.clone(),
            Arc::new(|| {}),
        )
        .tunnel_from(Arc::new(move || {
            // Configured for the first two reads — the one that fills
            // the row's cache and the one the toggle makes — and gone
            // by the time the start task looks.
            if counting.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
                Ok(Some((
                    Arc::new(FakeTunnel {
                        allocated: false,
                        url: "http://localhost:1".to_string(),
                        pending: false,
                        started: Default::default(),
                        stopped: Default::default(),
                    }) as Arc<dyn crate::cli::web::tunnel::Provider>,
                    std::time::Duration::from_secs(1),
                )))
            } else {
                Ok(None)
            }
        }));
        assert_eq!(r.shown(), Shown::Off, "the row saw a configured tunnel");

        let snap = snap();
        assert_eq!(r.toggle(), None, "the keypress had a tunnel to start");
        let notices = settle(&mut r, &snap).await;
        assert_eq!(r.shown(), Shown::Failed, "the fresh read refused it");
        let why = r.detail().unwrap();
        assert!(why.contains("No remote is configured"), "{why}");
        assert!(!notices.is_empty());
        assert!(rec.opened.lock().unwrap().is_empty(), "no browser");
        assert_eq!(
            crate::agent_store::web_port(repo.path()).unwrap(),
            None,
            "and nothing was bound"
        );
        r.stop().await;
    }

    /// A tunnel that IS configured but cannot be built — a retired
    /// provider, a config that will not parse — is an error to show,
    /// not an absence to misreport (codex on 5149876).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_source_that_errors_is_not_the_same_as_no_tunnel() {
        let repo = repo_with_config();
        let rec = Recorder::default();
        let mut r = Remote::new(
            repo.path().to_path_buf(),
            Some(repo.path().to_path_buf()),
            Arc::new(|| {
                Ok(Arc::new(FakePanes {
                    answers: true,
                    slow: std::time::Duration::ZERO,
                }) as Arc<dyn Panes>)
            }),
            Arc::new(Door::new(None)),
            crate::cli::web::PANE_POLL,
            crate::cli::web::STOP_GRACE,
            rec.clone(),
            Arc::new(|| {}),
        )
        .tunnel_from(Arc::new(|| {
            anyhow::bail!("the `ngrok` provider is gone: use `quick`")
        }));

        assert_eq!(r.shown(), Shown::Failed, "not `unconfigured`");
        let why = r.detail().unwrap();
        assert!(why.contains("is gone"), "the reason is shown: {why}");
        assert!(
            !why.contains("not configured") && !why.contains("No remote is configured"),
            "a broken tunnel is not an absent one: {why}"
        );

        // And throwing the switch reports the same reason rather
        // than telling the operator to add a tunnel they have.
        let snap = snap();
        assert_eq!(r.toggle(), None, "it is not refused as absent");
        settle(&mut r, &snap).await;
        assert_eq!(r.shown(), Shown::Failed);
        let why = r.detail().unwrap();
        assert!(why.contains("is gone"), "{why}");
        r.stop().await;
    }

    /// The switch refuses when no tunnel is configured, and says the
    /// config line to add: a page on loopback reaches nobody, which
    /// is the whole point of the remote.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unconfigured_remote_refuses_and_binds_nothing() {
        let repo = repo_with_config();
        let (mut r, rec) = {
            let rec = Recorder::default();
            let r = Remote::new(
                repo.path().to_path_buf(),
                None,
                Arc::new(|| {
                    Ok(Arc::new(FakePanes {
                        answers: true,
                        slow: std::time::Duration::ZERO,
                    }) as Arc<dyn Panes>)
                }),
                Arc::new(Door::new(None)),
                crate::cli::web::PANE_POLL,
                crate::cli::web::STOP_GRACE,
                rec.clone(),
                Arc::new(|| {}),
            )
            .tunnel_from(Arc::new(|| Ok(None)));
            (r, rec)
        };
        assert_eq!(r.shown(), Shown::Unconfigured);
        assert_eq!(r.detail().as_deref(), Some("not configured"));

        let notice = r.toggle().expect("the switch says why");
        assert!(notice.message.contains("remote.tunnel") || notice.message.contains("\"tunnel\""));
        assert!(notice.message.contains("quick"), "{}", notice.message);
        assert_eq!(r.shown(), Shown::Unconfigured, "and nothing started");
        assert!(rec.opened.lock().unwrap().is_empty(), "no browser");
        // Nothing was bound: the repo never had to remember a port.
        assert_eq!(crate::agent_store::web_port(repo.path()).unwrap(), None);
        r.stop().await;
    }

    /// A listing longer than the grace: the poll's task is aborted
    /// when the grace runs out, but the listing it was waiting for is
    /// waited for all the same — off returns only once it has ended,
    /// and nothing of the remote's is left running.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn off_waits_for_a_listing_past_the_grace() {
        use std::time::Duration;
        let repo = repo_with_config();
        let metrics = tokio::runtime::Handle::current().metrics();
        let baseline = metrics.num_alive_tasks();
        let (mut r, _) = remote_with(
            repo.path(),
            true,
            Duration::from_millis(1200),
            Duration::from_millis(10),
            Duration::from_millis(150),
        );
        let snap = snap();
        r.toggle();
        settle(&mut r, &snap).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let pressed = std::time::Instant::now();
        r.toggle();
        settle(&mut r, &snap).await;
        assert_eq!(r.shown(), Shown::Off);
        assert!(
            pressed.elapsed() >= Duration::from_millis(900),
            "the listing outlived the grace and was waited for: {:?}",
            pressed.elapsed()
        );
        let settled = std::time::Instant::now() + Duration::from_millis(300);
        while metrics.num_alive_tasks() > baseline && std::time::Instant::now() < settled {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(metrics.num_alive_tasks(), baseline, "nothing left running");
    }

    /// Off ends a poll that is in the middle of a slow listing: the
    /// task is aborted and joined, not asked and left — the runtime's
    /// live-task count is back where it was the moment off returns,
    /// while a cancel alone would leave the task until the listing
    /// came back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn off_joins_a_poll_in_flight() {
        use std::time::Duration;
        let repo = repo_with_config();
        let metrics = tokio::runtime::Handle::current().metrics();
        let baseline = metrics.num_alive_tasks();
        let (mut r, _) = remote_with(
            repo.path(),
            true,
            Duration::from_millis(600),
            Duration::from_millis(10),
            crate::cli::web::STOP_GRACE,
        );
        let snap = snap();
        r.toggle();
        settle(&mut r, &snap).await;
        // Past the first poll's sleep, so a listing is under way.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(metrics.num_alive_tasks() > baseline);
        let pressed = std::time::Instant::now();
        r.toggle();
        assert_eq!(r.shown(), Shown::Stopping);
        settle(&mut r, &snap).await;
        assert_eq!(r.shown(), Shown::Off);
        // The listing in flight was waited for — about its length —
        // and once off, every task of the remote's is gone.
        assert!(
            pressed.elapsed() >= Duration::from_millis(300),
            "waited for the listing"
        );
        let settled = std::time::Instant::now() + Duration::from_millis(300);
        while metrics.num_alive_tasks() > baseline && std::time::Instant::now() < settled {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            metrics.num_alive_tasks(),
            baseline,
            "off means every task of the remote's is joined"
        );
    }

    /// A provider whose "tunnel" is the loopback itself: its URL is
    /// wherever the test points it, and it records whether it was
    /// started and stopped. A `pending` one never finishes starting.
    struct FakeTunnel {
        url: String,
        /// True when the hostname is only known at start — the kind
        /// leased after discovery rather than claimed before.
        allocated: bool,
        pending: bool,
        started: Arc<std::sync::atomic::AtomicBool>,
        stopped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl crate::cli::web::tunnel::Provider for FakeTunnel {
        fn reservation(&self) -> Option<String> {
            self.allocated
                .then_some(())
                .map_or(Some(self.url.clone()), |_| None)
        }
        fn start(
            &self,
            _port: u16,
            _deadline: tokio::time::Instant,
        ) -> futures_util::future::BoxFuture<
            '_,
            anyhow::Result<(String, Box<dyn crate::cli::web::tunnel::Handle>)>,
        > {
            self.started
                .store(true, std::sync::atomic::Ordering::Relaxed);
            let stopped = self.stopped.clone();
            let pending = self.pending;
            let url = self.url.clone();
            Box::pin(async move {
                if pending {
                    std::future::pending::<()>().await;
                }
                Ok((
                    url,
                    Box::new(FakeHandle { stopped }) as Box<dyn crate::cli::web::tunnel::Handle>,
                ))
            })
        }
    }

    struct FakeHandle {
        stopped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl crate::cli::web::tunnel::Handle for FakeHandle {
        fn stop(self: Box<Self>) -> futures_util::future::BoxFuture<'static, Option<String>> {
            self.stopped
                .store(true, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { None })
        }
    }

    /// A remote with a tunnel provider, on a port decided now so the
    /// provider's fixed URL can name it — its own, unless `url` says
    /// otherwise — with a one-second grace.
    async fn tunnelled(
        home: &std::path::Path,
        url: Option<String>,
        pending: bool,
    ) -> (Remote<Recorder>, tempfile::TempDir, Arc<FakeTunnel>) {
        tunnelled_with(home, url, pending, false).await
    }

    async fn tunnelled_with(
        home: &std::path::Path,
        url: Option<String>,
        pending: bool,
        allocated: bool,
    ) -> (Remote<Recorder>, tempfile::TempDir, Arc<FakeTunnel>) {
        let repo = repo_with_config();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        crate::agent_store::record_web_port(repo.path(), port).unwrap();
        let provider = Arc::new(FakeTunnel {
            allocated,
            // The literal it actually binds, not a name: an
            // allocated endpoint is resolved at its own nameservers
            // now, and `localhost` has no zone to ask.
            url: url.unwrap_or_else(|| format!("http://127.0.0.1:{port}")),
            pending,
            started: Default::default(),
            stopped: Default::default(),
        });
        let source = provider.clone();
        let r = Remote::new(
            repo.path().to_path_buf(),
            Some(home.to_path_buf()),
            Arc::new(|| {
                Ok(Arc::new(FakePanes {
                    answers: true,
                    slow: std::time::Duration::ZERO,
                }) as Arc<dyn Panes>)
            }),
            Arc::new(Door::new(None)),
            crate::cli::web::PANE_POLL,
            crate::cli::web::STOP_GRACE,
            Recorder::default(),
            Arc::new(|| {}),
        )
        .tunnel_from(Arc::new(move || {
            Ok(Some((
                source.clone() as Arc<dyn crate::cli::web::tunnel::Provider>,
                std::time::Duration::from_secs(1),
            )))
        }));
        (r, repo, provider)
    }

    /// With a provider configured, on means the tunnel is leased,
    /// started and proven — the row shows the public URL — and off
    /// ends it and releases the endpoint. A second remote on the
    /// same endpoint is refused naming the holder and starts no
    /// tunnel; one whose URL is not proven is refused and the tunnel
    /// that was started is stopped. (Another remote's nonce is the
    /// probe's own test: an IP is no relying party.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tunnel_is_leased_proven_and_ends_with_the_remote() {
        use std::sync::atomic::Ordering::Relaxed;
        let home = tempfile::tempdir().unwrap();
        let snap = snap();
        let (mut a, _repo_a, tunnel_a) = tunnelled(home.path(), None, false).await;
        assert_eq!(a.toggle(), None);
        settle(&mut a, &snap).await;
        assert_eq!(a.shown(), Shown::On);
        assert_eq!(
            a.detail().as_deref(),
            Some(tunnel_a.url.as_str()),
            "the public URL"
        );
        assert!(tunnel_a.started.load(Relaxed) && !tunnel_a.stopped.load(Relaxed));

        // The same endpoint, claimed by another remote.
        let (mut b, _repo_b, tunnel_b) =
            tunnelled(home.path(), Some(tunnel_a.url.clone()), false).await;
        assert_eq!(b.toggle(), None);
        let notices = settle(&mut b, &snap).await;
        assert_eq!(b.shown(), Shown::Failed);
        let why = b.detail().unwrap();
        assert!(why.contains("held by"), "{why}");
        assert!(!notices.is_empty());
        assert!(
            !tunnel_b.started.load(Relaxed),
            "refused before anything started"
        );

        assert_eq!(a.toggle(), None);
        settle(&mut a, &snap).await;
        assert_eq!(a.shown(), Shown::Off);
        assert!(
            tunnel_a.stopped.load(Relaxed),
            "the tunnel ends with the remote"
        );

        // Released: b's start is refused by the probe now, not the
        // lease — nothing answers at a's old URL.
        assert_eq!(b.toggle(), None);
        settle(&mut b, &snap).await;
        assert_eq!(b.shown(), Shown::Failed);
        let why = b.detail().unwrap();
        assert!(why.contains("not reachable"), "{why}");
        assert!(
            tunnel_b.started.load(Relaxed) && tunnel_b.stopped.load(Relaxed),
            "the tunnel that was started is stopped"
        );
        b.stop().await;
    }

    /// An ALLOCATED endpoint — a hostname that does not exist until
    /// the tunnel is handed one — is leased after the start reports
    /// it, and a second remote on the same allocated name is refused
    /// by that lease. A CLAIMED endpoint whose tunnel comes up
    /// somewhere else is an error, not a hostname to adopt.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_allocated_endpoint_is_leased_once_the_start_names_it() {
        use std::sync::atomic::Ordering::Relaxed;
        let home = tempfile::tempdir().unwrap();
        let snap = snap();

        // Allocated: reservation() is None, so nothing is leased
        // before the start; the URL it reports is leased after.
        let (mut a, _repo_a, tunnel_a) = tunnelled_with(home.path(), None, false, true).await;
        assert!(
            crate::cli::web::tunnel::Provider::reservation(&*tunnel_a).is_none(),
            "nothing to claim up front"
        );
        assert_eq!(a.toggle(), None);
        settle(&mut a, &snap).await;
        assert_eq!(a.shown(), Shown::On);
        assert_eq!(a.detail().as_deref(), Some(tunnel_a.url.as_str()));
        assert!(tunnel_a.started.load(Relaxed));

        // A second remote handed the SAME allocated name: the lease
        // taken after discovery still excludes it, and its tunnel is
        // stopped rather than left running.
        let (mut b, _repo_b, tunnel_b) =
            tunnelled_with(home.path(), Some(tunnel_a.url.clone()), false, true).await;
        assert_eq!(b.toggle(), None);
        settle(&mut b, &snap).await;
        assert_eq!(b.shown(), Shown::Failed);
        let why = b.detail().unwrap();
        assert!(why.contains("held by"), "{why}");
        assert!(
            tunnel_b.started.load(Relaxed) && tunnel_b.stopped.load(Relaxed),
            "started to learn the name, then stopped"
        );
        a.stop().await;
        b.stop().await;
    }

    /// A claimed reservation the tunnel contradicts is refused, and
    /// what it did start is torn down.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reservation_the_tunnel_contradicts_is_an_error() {
        use std::sync::atomic::Ordering::Relaxed;
        let home = tempfile::tempdir().unwrap();
        let repo = repo_with_config();
        let rec = Recorder::default();
        // Claims one hostname, comes up on another.
        let provider = Arc::new(FakeTunnel {
            allocated: false,
            url: "http://localhost:9".to_string(),
            pending: false,
            started: Default::default(),
            stopped: Default::default(),
        });
        let claimed = Arc::new(Claiming {
            claim: "http://elsewhere.example:1".to_string(),
            inner: provider.clone(),
        });
        let mut r = Remote::new(
            repo.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            Arc::new(|| {
                Ok(Arc::new(FakePanes {
                    answers: true,
                    slow: std::time::Duration::ZERO,
                }) as Arc<dyn Panes>)
            }),
            Arc::new(Door::new(None)),
            crate::cli::web::PANE_POLL,
            crate::cli::web::STOP_GRACE,
            rec.clone(),
            Arc::new(|| {}),
        )
        .tunnel_from(Arc::new(move || {
            Ok(Some((
                claimed.clone() as Arc<dyn crate::cli::web::tunnel::Provider>,
                std::time::Duration::from_secs(2),
            )))
        }));
        let snap = snap();
        assert_eq!(r.toggle(), None);
        settle(&mut r, &snap).await;
        assert_eq!(r.shown(), Shown::Failed);
        let why = r.detail().unwrap();
        assert!(
            why.contains("reserved") && why.contains("came up on"),
            "{why}"
        );
        assert!(provider.stopped.load(Relaxed), "what started is stopped");
        r.stop().await;
    }

    /// Wraps a provider to claim a DIFFERENT endpoint than the one
    /// its start will report.
    struct Claiming {
        claim: String,
        inner: Arc<FakeTunnel>,
    }

    impl crate::cli::web::tunnel::Provider for Claiming {
        fn reservation(&self) -> Option<String> {
            Some(self.claim.clone())
        }
        fn start(
            &self,
            port: u16,
            deadline: tokio::time::Instant,
        ) -> futures_util::future::BoxFuture<
            '_,
            anyhow::Result<(String, Box<dyn crate::cli::web::tunnel::Handle>)>,
        > {
            self.inner.start(port, deadline)
        }
    }

    /// A provider that never finishes connecting: the start fails at
    /// the grace, and the exit path abandons it at once rather than
    /// waiting the grace out (codex on 6f60efa).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pending_tunnel_is_abandoned_by_the_grace_and_by_quitting() {
        let home = tempfile::tempdir().unwrap();
        let snap = snap();
        let (mut p, _repo_p, _tunnel_p) = tunnelled(home.path(), None, true).await;
        assert_eq!(p.toggle(), None);
        let started = std::time::Instant::now();
        settle(&mut p, &snap).await;
        assert_eq!(p.shown(), Shown::Failed);
        let why = p.detail().unwrap();
        assert!(why.contains("did not come up within 1s"), "{why}");
        // A provider that ignores its deadline is cut off by the
        // caller's backstop, one stop-bound later. Still bounded,
        // just not by the grace alone.
        assert!(
            started.elapsed()
                < std::time::Duration::from_secs(1)
                    + crate::cli::web::tunnel::STOP_BOUND
                    + std::time::Duration::from_secs(3),
            "bounded by the grace and the backstop: {:?}",
            started.elapsed()
        );
        p.stop().await;

        let (mut q, _repo_q, _tunnel_q) = tunnelled(home.path(), None, true).await;
        assert_eq!(q.toggle(), None);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(q.shown(), Shown::Starting);
        let quitting = std::time::Instant::now();
        q.stop().await;
        assert!(
            quitting.elapsed() < std::time::Duration::from_millis(800),
            "quitting does not wait out the connect: {:?}",
            quitting.elapsed()
        );
        assert_eq!(q.shown(), Shown::Off);
    }

    fn port_of(url: &str) -> u16 {
        url.rsplit(':').next().unwrap().parse().unwrap()
    }

    /// Wait for the switch to leave a transient state: the loop's
    /// drain, as the wake would drive it.
    async fn settle(
        r: &mut Remote<Recorder>,
        snap: &crate::cli::status::StatusSnapshot,
    ) -> Vec<Notice> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let notices = r.drain(snap);
            if !matches!(r.shown(), Shown::Starting | Shown::Stopping)
                || std::time::Instant::now() > deadline
            {
                return notices;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    fn snap() -> crate::cli::status::StatusSnapshot {
        crate::cli::status_tui::fixtures::snap(vec![], vec![])
    }

    /// The press returns at once and the row reads `starting…` while
    /// a slow zellij is listed off the loop; started, the page answers
    /// on the repo's port, seeded with the loop's snapshot, and the
    /// browser is sent there once. Off is a task too: `stopping…`,
    /// then nothing answers and every task is joined — an open
    /// `/events` stream is ended, not left. On again binds the same
    /// port. The TUI's `stop` ends it all the same.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_and_off_are_tasks_and_off_joins_everything() {
        use std::time::Duration;
        let repo = repo_with_config();
        let (mut r, rec) = remote_with(
            repo.path(),
            true,
            Duration::from_millis(300),
            crate::cli::web::PANE_POLL,
            crate::cli::web::STOP_GRACE,
        );
        let snap = snap();
        let pressed = std::time::Instant::now();
        assert_eq!(r.toggle(), None);
        assert!(
            pressed.elapsed() < Duration::from_millis(100),
            "not on the loop"
        );
        assert_eq!(r.shown(), Shown::Starting);
        assert!(settle(&mut r, &snap).await.is_empty());
        assert_eq!(r.shown(), Shown::On);
        let url = r.detail().unwrap();
        let port = port_of(&url);
        assert_eq!(
            crate::agent_store::web_port(repo.path()).unwrap(),
            Some(port)
        );
        // The browser is sent in through a login link: the page
        // itself asks for a session first.
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let opened = rec.opened.lock().unwrap().clone();
        assert_eq!(opened.len(), 1);
        assert!(
            opened[0].starts_with(&format!("{url}/login?t=")),
            "{opened:?}"
        );
        let welcome = client.get(&opened[0]).send().await.unwrap();
        assert_eq!(welcome.status(), 303);
        let cookie = welcome
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let page = client
            .get(&url)
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
        assert!(page.text().await.unwrap().contains("<title>clank</title>"));
        // Seeded: the socket opens with the snapshot's facts, before
        // any repository change. It is a WebSocket now, and it
        // carries the page's origin like any other request from it.
        let mut opening = String::new();
        let mut socket = {
            use futures_util::StreamExt;
            let request = tokio_tungstenite::tungstenite::handshake::client::Request::builder()
                .uri(format!("{}/events", url.replacen("http://", "ws://", 1)))
                .header("host", url.replacen("http://", "", 1))
                .header("origin", &url)
                .header("cookie", &cookie)
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header(
                    "sec-websocket-key",
                    tokio_tungstenite::tungstenite::handshake::client::generate_key(),
                )
                .body(())
                .unwrap();
            let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
            while !opening.contains("event: status") {
                let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
                    .await
                    .expect("a frame")
                    .expect("the retained state")
                    .unwrap();
                if let tokio_tungstenite::tungstenite::Message::Text(said) = frame {
                    opening.push_str(&said);
                }
            }
            socket
        };
        assert!(opening.contains("\"facts\""), "{opening}");
        r.open_again();
        assert_eq!(rec.opened.lock().unwrap().len(), 2);

        let pressed = std::time::Instant::now();
        assert_eq!(r.toggle(), None);
        assert!(pressed.elapsed() < Duration::from_millis(100));
        assert_eq!(r.shown(), Shown::Stopping);
        settle(&mut r, &snap).await;
        assert_eq!(r.shown(), Shown::Off);
        // Joined, not timed out: an off that needed the grace to abort
        // something was an off that left something running.
        assert!(
            pressed.elapsed() < Duration::from_secs(3),
            "off joined everything promptly: {:?}",
            pressed.elapsed()
        );
        assert!(reqwest::get(&url).await.is_err(), "nothing answers");
        // The socket that was open is over: its next read is the
        // end, not a wait.
        let ended = {
            use futures_util::StreamExt;
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    match socket.next().await {
                        None | Some(Err(_)) => return,
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => return,
                        Some(Ok(_)) => {}
                    }
                }
            })
            .await
        };
        assert!(
            ended.is_ok(),
            "the open socket ended with the remote rather than hanging"
        );
        let freed = std::net::TcpListener::bind(("127.0.0.1", port)).is_ok();

        r.toggle();
        settle(&mut r, &snap).await;
        assert_eq!(r.shown(), Shown::On);
        if freed {
            assert_eq!(r.detail().unwrap(), url, "the repo's port, again");
        }
        r.stop().await;
        assert_eq!(r.shown(), Shown::Off);
        assert!(reqwest::get(&url).await.is_err());
    }

    /// A zellij that does not answer is a failed start with that
    /// reason, nothing started, and the switch tries again on the
    /// next press.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_start_that_fails_says_why_and_leaves_nothing() {
        let repo = repo_with_config();
        let (mut r, rec) = remote(repo.path(), false);
        let snap = snap();
        assert_eq!(r.toggle(), None, "nothing to say until the task has tried");
        let notices = settle(&mut r, &snap).await;
        assert_eq!(notices.len(), 1);
        assert!(notices[0].message.contains("did not answer"), "{notices:?}");
        assert_eq!(r.shown(), Shown::Failed);
        assert_eq!(r.detail().as_deref(), Some(notices[0].message.as_str()));
        assert!(rec.opened.lock().unwrap().is_empty());
        assert_eq!(
            crate::agent_store::web_port(repo.path()).unwrap(),
            None,
            "the port is sampled only once zellij has answered"
        );
        r.toggle();
        assert_eq!(r.shown(), Shown::Starting, "failed → a new try");
        r.stop().await;
    }

    /// A browser that will not open is said, the remote staying on;
    /// the opener's word about an earlier start is nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unopenable_browser_is_said_and_the_remote_stays_on() {
        let repo = repo_with_config();
        let (mut r, rec) = remote(repo.path(), true);
        let snap = snap();
        r.toggle();
        settle(&mut r, &snap).await;
        (rec.reports.lock().unwrap()[0])(Outcome::BrowserFailed {
            why: "no opener".into(),
        });
        let notices = r.drain(&snap);
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0]
                .message
                .contains("could not open a browser: no opener")
        );
        // And it names the URL it actually tried. Naming a different
        // one sends the reader somewhere the feature did not go.
        let shown = r.detail().expect("still on");
        assert!(
            notices[0].message.starts_with(&shown),
            "the notice names the URL the row shows: {:?} vs {shown}",
            notices[0].message
        );
        assert_eq!(r.shown(), Shown::On);

        r.toggle();
        settle(&mut r, &snap).await;
        r.toggle();
        settle(&mut r, &snap).await;
        (rec.reports.lock().unwrap()[0])(Outcome::BrowserFailed { why: "late".into() });
        assert!(r.drain(&snap).is_empty(), "an earlier start's opener");
        r.stop().await;
    }
}
