//! The remote switch: the repo's page, served from inside the TUI as
//! one owned [`Instance`]. Switched on from the row, it binds the
//! port the repo remembers and the browser is sent there; switched
//! off, or the TUI closed, everything it started is ended and joined
//! (clank-tui-runs-the-remote-in-process).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::cli::web::{Instance, Panes};

/// What the bar and the row draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Shown {
    #[default]
    Off,
    Starting,
    On,
    Stopping,
    Failed,
}

/// What a task or thread of the switch's reported back: the start it
/// was asked for came up, or did not; the stop it was asked for is
/// done; the browser could not be opened.
pub(super) enum Outcome {
    Started(Instance),
    StartFailed { why: String },
    Stopped,
    BrowserFailed { why: String },
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

/// Starting and stopping are tasks in flight, held so the TUI's exit
/// can wait for them; the loop is never inside either (codex on
/// 55db154).
enum State {
    Off,
    Starting {
        task: tokio::task::JoinHandle<()>,
    },
    On {
        instance: Instance,
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
        poll: std::time::Duration,
        grace: std::time::Duration,
        opener: O,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            repo,
            home,
            panes,
            poll,
            grace,
            opener,
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
            State::Stopping { .. } => Shown::Stopping,
            State::Failed { .. } => Shown::Failed,
        }
    }

    /// What the row says beside the state: the URL when on, the
    /// reason when failed, nothing otherwise.
    pub(super) fn detail(&self) -> Option<String> {
        match &self.state {
            State::On { instance } => Some(instance.url.clone()),
            State::Failed { why } => Some(why.clone()),
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
                self.generation += 1;
                let mut report = self.reporter();
                let (repo, home, poll, grace) =
                    (self.repo.clone(), self.home.clone(), self.poll, self.grace);
                // Finding zellij is the task's too: it lists the session
                // synchronously, which is not the loop's to wait for
                // (codex on 369e74e).
                let panes = self.panes.clone();
                let task = tokio::spawn(async move {
                    let started = match tokio::task::spawn_blocking(move || panes()).await {
                        Ok(Ok(panes)) => Instance::start(repo, home, panes, poll, grace).await,
                        Ok(Err(e)) => Err(e),
                        Err(_) => Err(anyhow::anyhow!("finding zellij panicked")),
                    };
                    report(match started {
                        Ok(instance) => Outcome::Started(instance),
                        Err(e) => Outcome::StartFailed {
                            why: format!("{e:#}"),
                        },
                    });
                });
                self.state = State::Starting { task };
                None
            }
        }
    }

    /// The TUI rebuilt its snapshot; the page follows.
    pub(super) fn observe(&self, snap: &crate::cli::status::StatusSnapshot) {
        if let State::On { instance } = &self.state {
            instance.observe(snap);
        }
    }

    /// Send the browser to the URL again; nothing unless on.
    pub(super) fn open_again(&mut self) {
        if let State::On { instance } = &self.state {
            let url = instance.url.clone();
            let report = self.reporter();
            self.opener.open(&url, report);
        }
    }

    /// End the remote, everything joined: the TUI's exit path, where
    /// waiting is right. A start or stop in flight is waited for too,
    /// and a start that lands is stopped in turn.
    pub(super) async fn stop(&mut self) {
        match std::mem::replace(&mut self.state, State::Off) {
            State::On { instance } => instance.stop().await,
            State::Starting { task } | State::Stopping { task } => {
                let _ = task.await;
                for (_, outcome) in
                    std::mem::take(&mut *self.outcomes.lock().unwrap_or_else(|e| e.into_inner()))
                {
                    if let Outcome::Started(instance) = outcome {
                        instance.stop().await;
                    }
                }
            }
            State::Off | State::Failed { .. } => {}
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
                    let report = self.reporter();
                    self.opener.open(&instance.url, report);
                    self.state = State::On { instance };
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
                        message: format!("{}\n\ncould not open a browser: {why}", instance.url),
                    });
                }
                _ => {}
            }
        }
        notices
    }
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
    use super::*;
    use crate::cli::open_zellij::{PaneMeta, SubscribeChild};

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
    fn remote_with(
        repo: &std::path::Path,
        answers: bool,
        slow: std::time::Duration,
        poll: std::time::Duration,
        grace: std::time::Duration,
    ) -> (Remote<Recorder>, Recorder) {
        let rec = Recorder::default();
        let r = Remote::new(
            repo.to_path_buf(),
            None,
            Arc::new(move || {
                std::thread::sleep(slow);
                Ok(Arc::new(FakePanes { answers, slow }) as Arc<dyn Panes>)
            }),
            poll,
            grace,
            rec.clone(),
            Arc::new(|| {}),
        );
        (r, rec)
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
        let page = reqwest::get(&url).await.unwrap();
        assert_eq!(page.status(), 200);
        assert!(page.text().await.unwrap().contains("<title>clank</title>"));
        assert_eq!(*rec.opened.lock().unwrap(), vec![url.clone()]);
        // Seeded: the stream opens with the snapshot's facts, before
        // any repository change.
        let mut stream = reqwest::get(format!("{url}/events")).await.unwrap();
        let mut opening = String::new();
        while !opening.contains("event: status") {
            let chunk = tokio::time::timeout(Duration::from_secs(5), stream.chunk())
                .await
                .expect("a frame")
                .unwrap()
                .expect("the retained state");
            opening.push_str(&String::from_utf8_lossy(&chunk));
        }
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
        // The stream that was open is over: its next read is the end,
        // not a wait.
        let ended = tokio::time::timeout(Duration::from_secs(2), stream.chunk()).await;
        assert!(
            matches!(ended, Ok(Ok(None)) | Ok(Err(_))),
            "the open stream ended with the remote: {ended:?}"
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
