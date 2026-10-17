//! The owner side of `clank remote`: this TUI publishes its remote's
//! status, and serves each request through the same start and stop the
//! keyboard uses.

use crate::cli::remote_control::{self as rc, Control, Outcome, Want};
use std::path::{Path, PathBuf};

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
}

/// How long answered request and result files are kept.
const KEEP: std::time::Duration = std::time::Duration::from_secs(60);

pub(super) struct Served {
    repo: PathBuf,
    me: Option<rc::Owner>,
    control: Control,
    reason: Option<String>,
    in_flight: Option<(String, Want)>,
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
        written.updated = crate::cli::stop_hook::now_rfc3339();
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
        let answered = self.answered.contains(id) || rc::result_path(&self.repo, id).exists();
        let in_flight = self.in_flight.as_ref().map(|(id, _)| id.as_str());
        match rc::decide(&request, &me, remote.shown(), in_flight, answered) {
            rc::Decision::Ignore => {}
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
        self.progress(remote);
    }

    /// Answer the request in flight once the remote has finished.
    pub(super) fn progress(&mut self, remote: &impl Switch) {
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
            let thread = std::thread::spawn(move || {
                let mut served = Served::starting(&repo);
                let (tx, rx) = std::sync::mpsc::channel();
                let runtime = super::super::zellij::RuntimeWatch::start(&repo, tx, || {});
                let unavailable = match ready {
                    true => runtime.unavailable.clone(),
                    false => Some("the runtime directory cannot be watched".into()),
                };
                served.armed(unavailable, &*f.lock().unwrap());
                while !s.load(std::sync::atomic::Ordering::SeqCst) {
                    if let Ok(id) = rx.recv_timeout(Duration::from_millis(20)) {
                        served.request(&id, &mut *f.lock().unwrap(), &snap());
                    }
                    served.progress(&*f.lock().unwrap());
                }
            });
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
        let answer = rc::set(repo.path(), rc::Want::On, Duration::from_secs(10));
        assert_eq!(
            answer,
            rc::Answer::Done(Outcome::Applied {
                shown: rc::Shown::On
            })
        );
        assert_eq!(owner.fake.lock().unwrap().starts, 1);
        let again = rc::set(repo.path(), rc::Want::On, Duration::from_secs(10));
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
        let owner = Owner::serve(
            repo.path(),
            Fake {
                lands: Some(rc::Shown::Starting),
                ..Fake::default()
            },
            true,
        );
        let path = repo.path().to_path_buf();
        let first =
            std::thread::spawn(move || rc::set(&path, rc::Want::On, Duration::from_secs(10)));
        let started = std::time::Instant::now();
        while owner.fake.lock().unwrap().starts == 0 && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let second = rc::set(repo.path(), rc::Want::On, Duration::from_secs(10));
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
        let answer = rc::set(repo.path(), rc::Want::On, Duration::from_secs(10));
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
        let t = std::time::Instant::now();
        let published = loop {
            let raw = std::fs::read_to_string(rc::status_path(repo.path())).unwrap_or_default();
            if raw.contains("rejected") || t.elapsed() > Duration::from_secs(5) {
                break raw;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
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
        let t = std::time::Instant::now();
        let mut answer = rc::set(repo.path(), rc::Want::On, Duration::from_secs(10));
        // The owner's thread may not have published yet on the first look.
        while matches!(answer, rc::Answer::Unconfirmed(_)) && t.elapsed() < Duration::from_secs(5) {
            answer = rc::set(repo.path(), rc::Want::On, Duration::from_millis(200));
        }
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
        let answer = rc::set(repo.path(), rc::Want::On, Duration::from_millis(300));
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
        let path = repo.path().to_path_buf();
        let asked =
            std::thread::spawn(move || rc::set(&path, rc::Want::On, Duration::from_secs(10)));
        let t = std::time::Instant::now();
        while requests_written(repo.path()) == 0 && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
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
            want: rc::Want::On,
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
        let answer = rc::set_observed(repo.path(), rc::Want::On, deadline, &mut flood);
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
        let answer = rc::set_observed(repo.path(), rc::Want::On, deadline, &mut flood);
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
        let path = repo.path().to_path_buf();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let asked = std::thread::spawn(move || {
            rc::set_observed(&path, rc::Want::On, deadline, &mut hooked)
        });
        let t = std::time::Instant::now();
        let request = loop {
            let dir = rc::status_path(repo.path());
            let found = std::fs::read_dir(dir.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .find_map(|e| rc::request_id(repo.path(), &e.path()));
            if let Some(id) = found {
                break rc::read::<rc::Request>(&rc::request_path(repo.path(), &id)).unwrap();
            }
            assert!(t.elapsed() < Duration::from_secs(5), "no request written");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(request.owner, owner_b);
        a.kill().unwrap();
        a.wait().unwrap();
        std::thread::sleep(Duration::from_millis(300));
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
            rc::set(repo.path(), rc::Want::On, Duration::from_secs(1)),
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
}
