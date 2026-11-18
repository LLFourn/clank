//! The owner side of `clank remote`: this TUI publishes its remote's
//! status, and serves each request through the same start and stop the
//! keyboard uses.

use crate::cli::remote_control::{self as rc, Control, Outcome, Want};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Where a minted login link, or why there is none, is handed back.
pub(super) type Minted = Box<dyn FnOnce(anyhow::Result<String>) + Send>;

/// A link request's id, and its minted link or why there is none.
type MintedFor = (String, anyhow::Result<String>);

/// The remote as a request sees it.
pub(super) trait Switch {
    fn shown(&self) -> rc::Shown;
    /// Begin starting; `Some(why)` when it cannot start at all.
    fn switch_on(&mut self, snap: &crate::cli::status::StatusSnapshot) -> Option<String>;
    fn switch_off(&mut self);
    fn base_url(&self) -> Option<String>;
    fn failure(&self) -> Option<String>;
    /// Every credential this remote has held, for masking what it says.
    fn secrets(&self) -> Vec<String>;
    /// Mint the remote page's login link without waiting for it: `done`
    /// is called once, perhaps later and from another task.
    fn mint(&mut self, done: Minted);
}

/// How long answered request and result files are kept.
const KEEP: std::time::Duration = std::time::Duration::from_secs(60);

pub(super) struct Served {
    repo: PathBuf,
    me: Option<rc::Owner>,
    control: Control,
    reason: Option<String>,
    in_flight: Option<(String, Want)>,
    /// Link requests whose link is being minted.
    minting: std::collections::HashSet<String>,
    minted: Arc<Mutex<Vec<MintedFor>>>,
    answered: std::collections::HashSet<String>,
    published: Option<rc::Status>,
}

/// This process as an owner: its pid and native birth, when readable.
pub(super) fn me() -> Option<rc::Owner> {
    let pid = std::process::id() as i32;
    Some(rc::Owner {
        pid,
        birth: crate::proc_identity::token_for(pid)?,
    })
}

impl Served {
    /// Publishes `starting` at once, before the watch that serves
    /// requests exists, so no caller waits on a status that never comes.
    pub(super) fn starting(repo: &Path) -> Self {
        let mut served = Self {
            repo: repo.to_path_buf(),
            me: me(),
            control: Control::Starting,
            reason: None,
            in_flight: None,
            minting: Default::default(),
            minted: Default::default(),
            answered: Default::default(),
            published: None,
        };
        served.publish_shown(rc::Shown::Off, None, None, &[]);
        served
    }

    /// The watch is armed (`None`), or could not be (`Some(why)`).
    pub(super) fn armed(&mut self, unavailable: Option<String>, remote: &impl Switch) {
        (self.control, self.reason) = match (&self.me, unavailable) {
            (None, _) => (
                Control::Unavailable,
                Some("this clank tui cannot establish its own process identity".into()),
            ),
            (_, Some(why)) => (Control::Unavailable, Some(why)),
            (_, None) => (Control::Ready, None),
        };
        self.publish(remote);
    }

    /// Rewrite the status file when what it says has changed.
    pub(super) fn publish(&mut self, remote: &impl Switch) {
        self.publish_shown(
            remote.shown(),
            remote.base_url(),
            remote.failure(),
            &remote.secrets(),
        );
    }

    fn publish_shown(
        &mut self,
        shown: rc::Shown,
        url: Option<String>,
        error: Option<String>,
        secrets: &[String],
    ) {
        let Some(owner) = self.me.clone() else {
            return;
        };
        let status = rc::Status {
            protocol: rc::PROTOCOL,
            owner,
            control: self.control,
            control_reason: self.reason.as_deref().map(|r| rc::safe_reason(r, secrets)),
            shown,
            url: url.as_deref().and_then(rc::safe_url),
            error: error.as_deref().map(|e| rc::safe_reason(e, secrets)),
            updated: String::new(),
        };
        if self.published.as_ref() == Some(&status) {
            return;
        }
        let mut written = status.clone();
        written.updated = crate::age::now_rfc3339();
        if rc::write_atomic(&rc::status_path(&self.repo), &written).is_ok() {
            self.published = Some(status);
        }
    }

    /// A request file has appeared.
    pub(super) fn request(
        &mut self,
        id: &str,
        remote: &mut impl Switch,
        snap: &crate::cli::status::StatusSnapshot,
    ) {
        self.sweep();
        let Some(me) = self.me.clone() else {
            return;
        };
        let Some(request) = rc::read::<rc::Request>(&rc::request_path(&self.repo, id)) else {
            return;
        };
        let seen = self.answered.contains(id)
            || self.minting.contains(id)
            || self.in_flight.as_ref().is_some_and(|(at, _)| at == id)
            || rc::result_path(&self.repo, id).exists();
        match rc::admit(&request, &me, seen) {
            rc::Admit::Seen => {}
            rc::Admit::Stale => self.respond(id, Outcome::StaleOwner),
            rc::Admit::Act => match request.ask {
                rc::Ask::Switch { want } => self.switch(id, want, remote, snap),
                rc::Ask::Link => self.link(id, remote),
            },
        }
        self.progress(remote);
    }

    fn switch(
        &mut self,
        id: &str,
        want: Want,
        remote: &mut impl Switch,
        snap: &crate::cli::status::StatusSnapshot,
    ) {
        match rc::decide(want, remote.shown(), self.in_flight.is_some()) {
            rc::Decision::Answer(outcome) => self.respond(id, outcome),
            rc::Decision::Start => match remote.switch_on(snap) {
                Some(why) => self.respond(
                    id,
                    Outcome::Failed {
                        reason: rc::safe_reason(&why, &remote.secrets()),
                    },
                ),
                None => self.in_flight = Some((id.to_string(), Want::On)),
            },
            rc::Decision::Stop => {
                remote.switch_off();
                self.in_flight = Some((id.to_string(), Want::Off));
            }
        }
    }

    /// Answered from the state the remote is in now; a link is minted
    /// off the loop and answered by a later [`Self::progress`].
    fn link(&mut self, id: &str, remote: &mut impl Switch) {
        let failure = remote.failure();
        if let Some(why) = rc::unlinkable(remote.shown(), failure.as_deref()) {
            let reason = rc::safe_reason(&why, &remote.secrets());
            return self.respond(id, Outcome::Failed { reason });
        }
        self.minting.insert(id.to_string());
        let (id, minted) = (id.to_string(), self.minted.clone());
        remote.mint(Box::new(move |link| {
            minted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((id, link));
        }));
    }

    /// Answer the links minted since the last turn, and the request in
    /// flight once the remote has finished.
    pub(super) fn progress(&mut self, remote: &impl Switch) {
        let minted = std::mem::take(&mut *self.minted.lock().unwrap_or_else(|e| e.into_inner()));
        for (id, link) in minted {
            self.minting.remove(&id);
            // The CLI removes its request when it stops waiting: a link
            // written after that would rest in a file no one takes.
            if !rc::request_path(&self.repo, &id).exists() {
                self.answered.insert(id);
                continue;
            }
            let outcome = match link {
                Ok(link) => Outcome::Link { link },
                Err(e) => Outcome::Failed {
                    reason: rc::safe_reason(
                        &format!("could not mint a login link: {e:#}"),
                        &remote.secrets(),
                    ),
                },
            };
            self.respond(&id, outcome);
        }
        if let Some((id, want)) = self.in_flight.clone() {
            let failure = remote.failure();
            if let Some(outcome) = rc::completion(want, remote.shown(), failure.as_deref()) {
                let outcome = match outcome {
                    Outcome::Failed { reason } => Outcome::Failed {
                        reason: rc::safe_reason(&reason, &remote.secrets()),
                    },
                    other => other,
                };
                self.in_flight = None;
                self.respond(&id, outcome);
            }
        }
        self.publish(remote);
    }

    fn respond(&mut self, id: &str, outcome: Outcome) {
        let response = rc::Response {
            id: id.to_string(),
            outcome,
        };
        let _ = rc::write_atomic(&rc::result_path(&self.repo, id), &response);
        self.answered.insert(id.to_string());
    }

    /// Remove this repo's request and result files older than [`KEEP`].
    fn sweep(&mut self) {
        let dir = rc::status_path(&self.repo);
        let Some(dir) = dir.parent() else { return };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let req = rc::request_prefix(&self.repo);
        let res = req.replacen("remote-req-", "remote-res-", 1);
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !(name.starts_with(&req) || name.starts_with(&res)) {
                continue;
            }
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|age| age > KEEP);
            if old {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

impl<O: super::remote::Opener> Switch for super::remote::Remote<O> {
    fn shown(&self) -> rc::Shown {
        use super::remote::Shown as S;
        match super::remote::Remote::shown(self) {
            S::Off => rc::Shown::Off,
            S::Starting => rc::Shown::Starting,
            S::On => rc::Shown::On,
            S::Stopping => rc::Shown::Stopping,
            S::Failed => rc::Shown::Failed,
            S::Unconfigured => rc::Shown::Unconfigured,
        }
    }
    fn switch_on(&mut self, snap: &crate::cli::status::StatusSnapshot) -> Option<String> {
        super::remote::Remote::switch_on(self, snap).map(|notice| notice.message)
    }
    fn switch_off(&mut self) {
        super::remote::Remote::switch_off(self)
    }
    fn base_url(&self) -> Option<String> {
        super::remote::Remote::base_url(self)
    }
    fn failure(&self) -> Option<String> {
        super::remote::Remote::failure(self)
    }
    fn secrets(&self) -> Vec<String> {
        super::remote::Remote::secrets(self)
    }
    fn mint(&mut self, done: Minted) {
        super::remote::Remote::mint(self, done)
    }
}

/// One turn of the remote for the TUI loop: requests heard, the
/// remote's finished tasks taken in, the request in flight answered and
/// the status published. It runs before anything the loop draws, so an
/// overlay held open never holds a request's answer back; the notices
/// are returned for the loop to show when it can.
pub(super) async fn pump<O: super::remote::Opener>(
    served: &mut Served,
    remote: &mut super::remote::Remote<O>,
    requests: &std::sync::mpsc::Receiver<String>,
    snap: &crate::cli::status::StatusSnapshot,
) -> Vec<super::remote::Notice> {
    for id in requests.try_iter() {
        served.request(&id, remote, snap);
    }
    let notices = remote.drain(snap).await;
    served.progress(remote);
    notices
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::monitor::TempRepo;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// The remote, as a request sees it, scripted.
    #[derive(Default)]
    struct Fake {
        shown: Option<rc::Shown>,
        /// What a start becomes at once: `On`, or `Starting` to finish later.
        lands: Option<rc::Shown>,
        refuse: Option<String>,
        failure: Option<String>,
        secrets: Vec<String>,
        starts: usize,
        stops: usize,
        started: Option<std::sync::mpsc::Sender<()>>,
        /// What a mint answers at once; `None` holds it in `held`.
        minted: Option<Result<String, String>>,
        held: Vec<Minted>,
        mints: usize,
    }

    impl Switch for Fake {
        fn shown(&self) -> rc::Shown {
            self.shown.unwrap_or(rc::Shown::Off)
        }
        fn switch_on(&mut self, _: &crate::cli::status::StatusSnapshot) -> Option<String> {
            self.starts += 1;
            if let Some(why) = self.refuse.clone() {
                return Some(why);
            }
            self.shown = Some(self.lands.unwrap_or(rc::Shown::On));
            if let Some(started) = &self.started {
                let _ = started.send(());
            }
            None
        }
        fn switch_off(&mut self) {
            self.stops += 1;
            self.shown = Some(rc::Shown::Off);
        }
        fn base_url(&self) -> Option<String> {
            (self.shown() == rc::Shown::On)
                .then(|| "https://u:pw@host.example/x?t=SECRETTOKEN".into())
        }
        fn failure(&self) -> Option<String> {
            self.failure.clone()
        }
        fn secrets(&self) -> Vec<String> {
            self.secrets.clone()
        }
        fn mint(&mut self, done: Minted) {
            self.mints += 1;
            match self.minted.clone() {
                Some(Ok(link)) => done(Ok(link)),
                Some(Err(why)) => done(Err(anyhow::anyhow!(why))),
                None => self.held.push(done),
            }
        }
    }

    fn snap() -> crate::cli::status::StatusSnapshot {
        crate::cli::status_tui::fixtures::snap(None, vec![])
    }

    /// A TUI's serving half on its own thread: the real lease, the real
    /// runtime watch and dispatch, `Served`, and a fake remote.
    struct Owner {
        fake: Arc<Mutex<Fake>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Owner {
        fn serve(repo: &Path, fake: Fake, ready: bool) -> Self {
            let fake = Arc::new(Mutex::new(fake));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let repo = repo.to_path_buf();
            let (f, s) = (fake.clone(), stop.clone());
            let (armed, ready_rx) = std::sync::mpsc::sync_channel(0);
            let thread = std::thread::spawn(move || {
                let mut served = Served::starting(&repo);
                let (tx, rx) = std::sync::mpsc::channel();
                let runtime = super::super::zellij::RuntimeWatch::start(&repo, tx, || {});
                let unavailable = match ready {
                    true => runtime.unavailable.clone(),
                    false => Some("the runtime directory cannot be watched".into()),
                };
                served.armed(unavailable, &*f.lock().unwrap());
                armed.send(()).unwrap();
                while !s.load(std::sync::atomic::Ordering::SeqCst) {
                    if let Ok(id) = rx.recv_timeout(Duration::from_millis(20)) {
                        served.request(&id, &mut *f.lock().unwrap(), &snap());
                    }
                    served.progress(&*f.lock().unwrap());
                }
            });
            ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Self {
                fake,
                stop,
                thread: Some(thread),
            }
        }
    }

    impl Drop for Owner {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn requests_written(repo: &Path) -> usize {
        let dir = rc::status_path(repo);
        std::fs::read_dir(dir.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| rc::request_id(repo, &e.path()).is_some())
            .count()
    }

    #[test]
    fn a_request_reaches_its_tui_and_is_applied_when_the_remote_is_on() {
        let repo = TempRepo::new();
        let _lease = super::super::lease::acquire(repo.path()).unwrap();
        let owner = Owner::serve(repo.path(), Fake::default(), true);
        let answer = rc::ask(repo.path(), rc::Want::On, Duration::from_secs(10));
        assert_eq!(
            answer,
            rc::Answer::Done(Outcome::Applied {
                shown: rc::Shown::On
            })
        );
        assert_eq!(owner.fake.lock().unwrap().starts, 1);
        let again = rc::ask(repo.path(), rc::Want::On, Duration::from_secs(10));
        assert_eq!(
            again,
            rc::Answer::Done(Outcome::Applied {
                shown: rc::Shown::On
            })
        );
        assert_eq!(
            owner.fake.lock().unwrap().starts,
            1,
            "already on: nothing restarted"
        );
        let status: rc::Status = rc::read(&rc::status_path(repo.path())).unwrap();
        assert_eq!(
            status.url.as_deref(),
            Some("https://host.example/x"),
            "a base address only"
        );
        let published = std::fs::read_to_string(rc::status_path(repo.path())).unwrap();
        assert!(!published.contains("SECRETTOKEN") && !published.contains("pw@"));
        assert!(
            !rc::request_path(repo.path(), "x").starts_with(repo.path()),
            "never in the repo"
        );
    }

    #[test]
    fn applied_waits_for_the_transition_and_a_second_caller_is_busy() {
        let repo = TempRepo::new();
        let _lease = super::super::lease::acquire(repo.path()).unwrap();
        let (started, seen) = std::sync::mpsc::channel();
        let owner = Owner::serve(
            repo.path(),
            Fake {
                lands: Some(rc::Shown::Starting),
                started: Some(started),
                ..Fake::default()
            },
            true,
        );
        let path = repo.path().to_path_buf();
        let first =
            std::thread::spawn(move || rc::ask(&path, rc::Want::On, Duration::from_secs(10)));
        seen.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = rc::ask(repo.path(), rc::Want::On, Duration::from_secs(10));
        assert_eq!(
            second,
            rc::Answer::Done(Outcome::Busy {
                shown: rc::Shown::Starting
            })
        );
        assert!(!first.is_finished(), "starting is not done");
        owner.fake.lock().unwrap().shown = Some(rc::Shown::On);
        assert_eq!(
            first.join().unwrap(),
            rc::Answer::Done(Outcome::Applied {
                shown: rc::Shown::On
            })
        );
    }

    /// A start failure quoting a login link with a short token, a
    /// URL's userinfo and a known short connector credential.
    const LEAKY: &str = "connector credential abc12 rejected; \
        could not reach https://u:pw@host.example/login?t=SECRETTOKEN.";

    fn leaks(text: &str) -> bool {
        ["SECRETTOKEN", "abc12", "pw@", "/login"]
            .iter()
            .any(|s| text.contains(s))
    }

    #[test]
    fn a_failed_start_is_reported_without_its_secrets() {
        let repo = TempRepo::new();
        let _lease = super::super::lease::acquire(repo.path()).unwrap();
        let _owner = Owner::serve(
            repo.path(),
            Fake {
                refuse: Some(LEAKY.into()),
                secrets: vec!["abc12".into()],
                ..Fake::default()
            },
            true,
        );
        let answer = rc::ask(repo.path(), rc::Want::On, Duration::from_secs(10));
        let rc::Answer::Done(Outcome::Failed { reason }) = &answer else {
            panic!("{answer:?}");
        };
        assert!(!leaks(reason), "{reason}");
        assert!(
            reason.contains("rejected") && reason.contains("could not reach https://host.example"),
            "the failure still says what failed, and where: {reason}"
        );
        assert!(!leaks(&answer.to_string()), "{answer}");
    }

    #[test]
    fn a_failed_remote_publishes_its_reason_without_its_secrets() {
        let repo = TempRepo::new();
        let _lease = super::super::lease::acquire(repo.path()).unwrap();
        let _owner = Owner::serve(
            repo.path(),
            Fake {
                shown: Some(rc::Shown::Failed),
                failure: Some(LEAKY.into()),
                secrets: vec!["abc12".into()],
                ..Fake::default()
            },
            true,
        );
        let published = std::fs::read_to_string(rc::status_path(repo.path())).unwrap();
        assert!(published.contains("rejected"), "{published}");
        assert!(!leaks(&published), "{published}");
        let shown = rc::status_text(repo.path());
        assert!(shown.contains("error: ") && !leaks(&shown), "{shown}");
    }

    #[test]
    fn an_unavailable_owner_is_refused_at_once_with_its_reason() {
        let repo = TempRepo::new();
        let _lease = super::super::lease::acquire(repo.path()).unwrap();
        let _owner = Owner::serve(repo.path(), Fake::default(), false);
        let answer = rc::ask(repo.path(), rc::Want::On, Duration::from_secs(10));
        assert!(
            matches!(&answer, rc::Answer::Unavailable { reason, .. } if reason.contains("cannot be watched")),
            "{answer:?}"
        );
        assert_eq!(requests_written(repo.path()), 0);
    }

    #[test]
    fn a_current_owner_that_has_not_published_is_waited_for_never_called_older() {
        let repo = TempRepo::new();
        let _lease = super::super::lease::acquire(repo.path()).unwrap();
        let answer = rc::ask(repo.path(), rc::Want::On, Duration::from_millis(300));
        assert!(
            matches!(&answer, rc::Answer::Unconfirmed(why) if why.contains("did not become ready")),
            "{answer:?}"
        );
        assert_eq!(requests_written(repo.path()), 0);
    }

    #[test]
    fn an_owner_that_dies_before_answering_leaves_it_unconfirmed() {
        let repo = TempRepo::new();
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let birth = crate::proc_identity::token_for(pid).unwrap();
        let _lease = super::super::lease::hold_as(
            repo.path(),
            &rc::Holder {
                pid,
                birth: Some(birth.clone()),
                remote_protocol: Some(rc::PROTOCOL),
            },
        );
        let status = rc::Status {
            protocol: rc::PROTOCOL,
            owner: rc::Owner { pid, birth },
            control: Control::Ready,
            control_reason: None,
            shown: rc::Shown::Off,
            url: None,
            error: None,
            updated: String::new(),
        };
        rc::write_atomic(&rc::status_path(repo.path()), &status).unwrap();
        let (tx, written) = std::sync::mpsc::channel();
        let _watch = super::super::zellij::RuntimeWatch::start(repo.path(), tx, || {});
        assert!(_watch.unavailable.is_none());
        let path = repo.path().to_path_buf();
        let asked =
            std::thread::spawn(move || rc::ask(&path, rc::Want::On, Duration::from_secs(10)));
        written.recv_timeout(Duration::from_secs(5)).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        let answer = asked.join().unwrap();
        assert!(
            matches!(&answer, rc::Answer::Unconfirmed(why) if why.contains("exited before answering")),
            "{answer:?}"
        );
    }

    #[test]
    fn a_request_seen_twice_is_applied_once() {
        let repo = TempRepo::new();
        let mut served = Served::starting(repo.path());
        let mut fake = Fake::default();
        served.armed(None, &fake);
        let request = rc::Request {
            id: "dup-1".into(),
            ask: rc::Want::On.into(),
            owner: me().unwrap(),
        };
        rc::write_atomic(&rc::request_path(repo.path(), "dup-1"), &request).unwrap();
        served.request("dup-1", &mut fake, &snap());
        served.request("dup-1", &mut fake, &snap());
        assert_eq!(fake.starts, 1);
    }

    /// Wakes the CLI forever without any answer: the directory of a
    /// busy machine. Woken past the deadline is the bug.
    struct Flood {
        deadline: std::time::Instant,
        woken: usize,
    }

    impl rc::Observe for Flood {
        fn arm(&mut self, _: &rc::Owner) -> crate::proc_identity::exit_watch::Armed {
            crate::proc_identity::exit_watch::Armed::Watching
        }
        fn next(&mut self, _: std::time::Instant) -> Option<Vec<rc::Owner>> {
            assert!(
                std::time::Instant::now() < self.deadline + Duration::from_secs(2),
                "still waiting long after the deadline"
            );
            self.woken += 1;
            Some(Vec::new())
        }
    }

    #[test]
    fn the_deadline_holds_while_the_directory_never_goes_quiet() {
        let repo = TempRepo::new();
        let _lease = super::super::lease::acquire(repo.path()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        let mut flood = Flood { deadline, woken: 0 };
        let answer = rc::ask_observed(repo.path(), rc::Want::On.into(), deadline, &mut flood);
        assert!(
            matches!(&answer, rc::Answer::Unconfirmed(why) if why.contains("did not become ready")),
            "{answer:?}"
        );
        assert!(flood.woken > 0);
        assert_eq!(requests_written(repo.path()), 0, "nothing asked once late");

        let mut served = Served::starting(repo.path());
        served.armed(None, &Fake::default());
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        let mut flood = Flood { deadline, woken: 0 };
        let answer = rc::ask_observed(repo.path(), rc::Want::On.into(), deadline, &mut flood);
        assert!(
            matches!(&answer, rc::Answer::Unconfirmed(why) if why.contains("did not answer")),
            "{answer:?}"
        );
        assert!(flood.woken > 0);
    }

    /// The real exit watch, with a hook run as each owner is armed.
    struct Hooked {
        inner: rc::Watched,
        on_arm: Box<dyn FnMut(&rc::Owner) + Send>,
    }

    impl rc::Observe for Hooked {
        fn arm(&mut self, owner: &rc::Owner) -> crate::proc_identity::exit_watch::Armed {
            let armed = self.inner.arm(owner);
            (self.on_arm)(owner);
            armed
        }
        fn next(&mut self, until: std::time::Instant) -> Option<Vec<rc::Owner>> {
            self.inner.next(until)
        }
    }

    fn sleeper() -> (std::process::Child, rc::Owner) {
        let child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let birth = crate::proc_identity::token_for(pid).unwrap();
        (child, rc::Owner { pid, birth })
    }

    fn ready_status(owner: &rc::Owner) -> rc::Status {
        rc::Status {
            protocol: rc::PROTOCOL,
            owner: owner.clone(),
            control: Control::Ready,
            control_reason: None,
            shown: rc::Shown::Off,
            url: None,
            error: None,
            updated: String::new(),
        }
    }

    fn hold(repo: &Path, owner: &rc::Owner) -> super::super::lease::StatusLease {
        super::super::lease::hold_as(
            repo,
            &rc::Holder {
                pid: owner.pid,
                birth: Some(owner.birth.clone()),
                remote_protocol: Some(rc::PROTOCOL),
            },
        )
    }

    /// Owner A is armed; before the CLI reads again, B has taken the
    /// lease and published ready. The request goes to B, A's exit says
    /// nothing about it, and B's exit ends the wait.
    #[test]
    fn an_owner_replaced_before_the_request_is_the_one_watched() {
        let repo = TempRepo::new();
        let (mut a, owner_a) = sleeper();
        let (mut b, owner_b) = sleeper();
        let lease = Arc::new(Mutex::new(Some(hold(repo.path(), &owner_a))));
        rc::write_atomic(&rc::status_path(repo.path()), &ready_status(&owner_a)).unwrap();
        let mut hooked = Hooked {
            inner: rc::Watched::new(repo.path()).unwrap(),
            on_arm: {
                let (repo, lease, a, b) = (
                    repo.path().to_path_buf(),
                    lease.clone(),
                    owner_a.clone(),
                    owner_b.clone(),
                );
                Box::new(move |armed| {
                    if *armed == a {
                        let mut held = lease.lock().unwrap();
                        drop(held.take());
                        *held = Some(hold(&repo, &b));
                        rc::write_atomic(&rc::status_path(&repo), &ready_status(&b)).unwrap();
                    }
                })
            },
        };
        let (tx, written) = std::sync::mpsc::channel();
        let _watch = super::super::zellij::RuntimeWatch::start(repo.path(), tx, || {});
        assert!(_watch.unavailable.is_none());
        let path = repo.path().to_path_buf();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let asked = std::thread::spawn(move || {
            rc::ask_observed(&path, rc::Want::On.into(), deadline, &mut hooked)
        });
        let id = written.recv_timeout(Duration::from_secs(5)).unwrap();
        let request = rc::read::<rc::Request>(&rc::request_path(repo.path(), &id)).unwrap();
        assert_eq!(request.owner, owner_b);
        a.kill().unwrap();
        a.wait().unwrap();
        assert!(!asked.is_finished(), "the retired owner's exit is not B's");
        b.kill().unwrap();
        b.wait().unwrap();
        let answer = asked.join().unwrap();
        assert!(
            matches!(&answer, rc::Answer::Unconfirmed(why)
                if why.contains(&format!("pid {}) exited before answering", owner_b.pid))),
            "B's exit is heard: {answer:?}"
        );
    }

    #[test]
    fn an_owner_speaking_another_protocol_is_refused_without_a_request() {
        let repo = TempRepo::new();
        let me = me().unwrap();
        let _lease = super::super::lease::hold_as(
            repo.path(),
            &rc::Holder {
                pid: me.pid,
                birth: Some(me.birth.clone()),
                remote_protocol: Some(rc::PROTOCOL + 1),
            },
        );
        assert_eq!(
            rc::ask(repo.path(), rc::Want::On, Duration::from_secs(1)),
            rc::Answer::Unsupported {
                pid: me.pid,
                protocol: rc::PROTOCOL + 1
            }
        );
        assert_eq!(requests_written(repo.path()), 0);
    }

    #[test]
    fn a_status_no_current_owner_published_is_not_shown() {
        let repo = TempRepo::new();
        crate::cli::monitor::ensure_runtime_dir().unwrap();
        let mut left = ready_status(&me().unwrap());
        left.shown = rc::Shown::On;
        left.url = Some("https://previous-owner.example".into());
        rc::write_atomic(&rc::status_path(repo.path()), &left).unwrap();
        assert_eq!(rc::status_text(repo.path()), "owner: none");
        let (mut child, other) = sleeper();
        let _lease = hold(repo.path(), &other);
        let shown = rc::status_text(repo.path());
        assert!(
            shown.contains(&format!("pid {}", other.pid)) && !shown.contains("previous-owner"),
            "{shown}"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    const LINK: &str = "https://host.example/x/login?t=LINKTOKEN";

    fn on(minted: Option<Result<String, String>>) -> Fake {
        Fake {
            shown: Some(rc::Shown::On),
            minted,
            ..Fake::default()
        }
    }

    fn ask_for(repo: &Path, id: &str, ask: rc::Ask, owner: rc::Owner) {
        let request = rc::Request {
            id: id.into(),
            ask,
            owner,
        };
        rc::write_atomic(&rc::request_path(repo, id), &request).unwrap();
    }

    fn result(repo: &Path, id: &str) -> Option<Outcome> {
        rc::read::<rc::Response>(&rc::result_path(repo, id)).map(|r| r.outcome)
    }

    #[test]
    fn a_link_is_answered_from_the_on_remote_without_switching_it() {
        use std::os::unix::fs::PermissionsExt;
        let repo = TempRepo::new();
        let mut served = Served::starting(repo.path());
        let mut fake = on(Some(Ok(LINK.into())));
        served.armed(None, &fake);
        ask_for(repo.path(), "link-1", rc::Ask::Link, me().unwrap());
        served.request("link-1", &mut fake, &snap());
        assert_eq!(
            result(repo.path(), "link-1"),
            Some(Outcome::Link { link: LINK.into() })
        );
        assert_eq!((fake.starts, fake.stops, fake.mints), (0, 0, 1));
        assert!(served.in_flight.is_none());
        let mode = std::fs::metadata(rc::result_path(repo.path(), "link-1"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        let published = std::fs::read_to_string(rc::status_path(repo.path())).unwrap();
        assert!(published.contains("\"on\""), "{published}");
        assert!(
            !published.contains("LINKTOKEN") && !published.contains("login"),
            "{published}"
        );
    }

    #[test]
    fn a_link_while_off_says_to_switch_on_and_starts_nothing() {
        let repo = TempRepo::new();
        let mut served = Served::starting(repo.path());
        let mut fake = Fake {
            minted: Some(Ok(LINK.into())),
            ..Fake::default()
        };
        served.armed(None, &fake);
        ask_for(repo.path(), "link-off", rc::Ask::Link, me().unwrap());
        served.request("link-off", &mut fake, &snap());
        let Some(Outcome::Failed { reason }) = result(repo.path(), "link-off") else {
            panic!("{:?}", result(repo.path(), "link-off"));
        };
        assert!(reason.contains("clank remote on"), "{reason}");
        assert_eq!((fake.starts, fake.mints), (0, 0));
    }

    #[test]
    fn a_link_request_for_another_owner_is_stale_and_mints_nothing() {
        let repo = TempRepo::new();
        let mut served = Served::starting(repo.path());
        let mut fake = on(Some(Ok(LINK.into())));
        served.armed(None, &fake);
        let mut previous = me().unwrap();
        previous.birth = crate::proc_identity::ProcToken::MacStart { sec: 0, usec: 9 };
        ask_for(repo.path(), "link-stale", rc::Ask::Link, previous);
        served.request("link-stale", &mut fake, &snap());
        assert_eq!(result(repo.path(), "link-stale"), Some(Outcome::StaleOwner));
        assert_eq!(fake.mints, 0);
    }

    /// The link is minted off the loop: while it is, the same request
    /// heard again is not minted again, and another request is answered.
    #[test]
    fn a_link_being_minted_is_minted_once_and_holds_nothing_else_back() {
        let repo = TempRepo::new();
        let mut served = Served::starting(repo.path());
        let mut fake = on(None);
        served.armed(None, &fake);
        ask_for(repo.path(), "link-dup", rc::Ask::Link, me().unwrap());
        served.request("link-dup", &mut fake, &snap());
        served.request("link-dup", &mut fake, &snap());
        assert_eq!(fake.mints, 1);
        assert_eq!(result(repo.path(), "link-dup"), None);
        ask_for(
            repo.path(),
            "on-meanwhile",
            rc::Want::On.into(),
            me().unwrap(),
        );
        served.request("on-meanwhile", &mut fake, &snap());
        assert_eq!(
            result(repo.path(), "on-meanwhile"),
            Some(Outcome::Applied {
                shown: rc::Shown::On
            })
        );
        let done = fake.held.pop().unwrap();
        done(Ok(LINK.into()));
        assert_eq!(result(repo.path(), "link-dup"), None, "taken by a turn");
        served.progress(&fake);
        assert_eq!(
            result(repo.path(), "link-dup"),
            Some(Outcome::Link { link: LINK.into() })
        );
        std::fs::remove_file(rc::result_path(repo.path(), "link-dup")).unwrap();
        served.request("link-dup", &mut fake, &snap());
        assert_eq!(fake.mints, 1);
        assert_eq!(result(repo.path(), "link-dup"), None, "answered once");
    }

    #[test]
    fn a_link_minted_after_the_cli_left_is_not_written() {
        let repo = TempRepo::new();
        let mut served = Served::starting(repo.path());
        let mut fake = on(None);
        served.armed(None, &fake);
        ask_for(repo.path(), "link-late", rc::Ask::Link, me().unwrap());
        served.request("link-late", &mut fake, &snap());
        std::fs::remove_file(rc::request_path(repo.path(), "link-late")).unwrap();
        fake.held.pop().unwrap()(Ok(LINK.into()));
        served.progress(&fake);
        assert!(!rc::result_path(repo.path(), "link-late").exists());
    }

    #[test]
    fn the_switch_in_flight_heard_again_is_not_answered_busy() {
        let repo = TempRepo::new();
        let mut served = Served::starting(repo.path());
        let mut fake = Fake {
            lands: Some(rc::Shown::Starting),
            ..Fake::default()
        };
        served.armed(None, &fake);
        ask_for(repo.path(), "on-slow", rc::Want::On.into(), me().unwrap());
        served.request("on-slow", &mut fake, &snap());
        served.request("on-slow", &mut fake, &snap());
        assert_eq!(fake.starts, 1);
        assert_eq!(
            result(repo.path(), "on-slow"),
            None,
            "its own result is owed"
        );
    }

    #[test]
    fn a_mint_that_fails_is_a_failure_never_the_base_url() {
        let repo = TempRepo::new();
        let mut served = Served::starting(repo.path());
        let mut fake = Fake {
            secrets: vec!["abc12".into()],
            ..on(Some(Err(LEAKY.into())))
        };
        served.armed(None, &fake);
        ask_for(repo.path(), "link-fails", rc::Ask::Link, me().unwrap());
        served.request("link-fails", &mut fake, &snap());
        let Some(Outcome::Failed { reason }) = result(repo.path(), "link-fails") else {
            panic!("{:?}", result(repo.path(), "link-fails"));
        };
        assert!(reason.contains("could not mint a login link"), "{reason}");
        assert!(!leaks(&reason), "{reason}");
    }

    #[test]
    fn the_cli_prints_the_link_and_leaves_no_file_holding_it() {
        let repo = TempRepo::new();
        let _lease = super::super::lease::acquire(repo.path()).unwrap();
        let _owner = Owner::serve(repo.path(), on(Some(Ok(LINK.into()))), true);
        let answer = rc::ask(repo.path(), rc::Ask::Link, Duration::from_secs(10));
        assert_eq!(
            answer,
            rc::Answer::Done(Outcome::Link { link: LINK.into() })
        );
        assert!(!answer.to_string().contains("LINKTOKEN"), "{answer}");
        let dir = rc::status_path(repo.path());
        let ours = rc::request_prefix(repo.path()).replacen("remote-req-", "", 1);
        let held: Vec<_> = std::fs::read_dir(dir.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(&ours))
            .filter(|e| std::fs::read_to_string(e.path()).is_ok_and(|s| s.contains("LINKTOKEN")))
            .map(|e| e.path())
            .collect();
        assert!(held.is_empty(), "{held:?}");
    }

    /// The protocol of TUIs built before link requests.
    const LINKLESS: u32 = 1;

    #[test]
    fn an_owner_on_the_previous_protocol_is_told_to_restart() {
        let repo = TempRepo::new();
        let me = me().unwrap();
        let _lease = super::super::lease::hold_as(
            repo.path(),
            &rc::Holder {
                pid: me.pid,
                birth: Some(me.birth.clone()),
                remote_protocol: Some(LINKLESS),
            },
        );
        let answer = rc::ask(repo.path(), rc::Ask::Link, Duration::from_secs(10));
        assert_eq!(
            answer,
            rc::Answer::Unsupported {
                pid: me.pid,
                protocol: LINKLESS
            }
        );
        assert!(answer.to_string().contains("restart"), "{answer}");
        assert_eq!(requests_written(repo.path()), 0);
    }
}
