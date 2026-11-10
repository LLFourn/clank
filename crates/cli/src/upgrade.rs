//! What a long-lived process does about a newer clank being installed.
//!
//! One [`Authority`] per process holds the answer to two questions: may
//! this process still write shared durable state, and is it at a point
//! where replacing itself loses nothing. They are one question. Work that
//! must be finished holds a [`Permit`]; a different build being installed
//! refuses every new permit; and the process is replaceable exactly when a
//! different build is installed and no permit is out.
//!
//! A process that never arms its authority is a one-shot command: it is
//! always current, and nothing here costs it a system call.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use crate::build_stamp::{Build, Install, Installation, Stamp};

/// How often a long-lived process looks at its installation when nothing
/// else prompts it to.
pub const BEAT: Duration = Duration::from_secs(5);

/// What a display process shows while it runs a build that is no longer
/// the installed one.
pub const STALE_BADGE: &str = "newer clank installed · restart to upgrade";

/// Where a process stands against its installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// It runs the installed build, or nothing proves otherwise.
    Current,
    /// A different build is installed, and work begun before that was
    /// seen is unfinished: a presentation on its way, a result owed.
    Quiescing { installed: Build, in_flight: usize },
    /// A different build is installed and nothing is in flight: the safe
    /// point, at which replacing this process loses nothing.
    Replaceable { installed: Build },
}

impl Standing {
    /// The build installed in place of this one, if one is.
    pub fn stale(&self) -> Option<&Build> {
        match self {
            Standing::Current => None,
            Standing::Quiescing { installed, .. } | Standing::Replaceable { installed } => {
                Some(installed)
            }
        }
    }
}

/// Why a write was refused: this process has seen `installed` take its
/// build's place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stale {
    pub installed: Build,
}

impl std::fmt::Display for Stale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{STALE_BADGE}: build {} is installed in place of this process's, which no longer \
             writes shared state",
            self.installed.stamp
        )
    }
}

impl std::error::Error for Stale {}

impl From<Stale> for std::io::Error {
    fn from(stale: Stale) -> Self {
        std::io::Error::other(stale)
    }
}

#[derive(Debug)]
struct Inner {
    watched: Option<(Installation, Stamp)>,
    seen: Option<Install>,
    /// The latest proof of replacement. An installation that cannot be
    /// read proves nothing, so it neither sets nor clears this.
    installed: Option<Build>,
    in_flight: usize,
    refused: u64,
}

impl Inner {
    fn look(&mut self) {
        let Some((installation, running)) = &mut self.watched else {
            return;
        };
        let install = installation.look(*running);
        match &install {
            Install::Current => self.installed = None,
            Install::Replaced(build) => self.installed = Some(build.clone()),
            Install::Unavailable(_) => {}
        }
        self.seen = Some(install);
    }

    fn standing(&self) -> Standing {
        match (&self.installed, self.in_flight) {
            (None, _) => Standing::Current,
            (Some(installed), 0) => Standing::Replaceable {
                installed: installed.clone(),
            },
            (Some(installed), in_flight) => Standing::Quiescing {
                installed: installed.clone(),
                in_flight,
            },
        }
    }
}

/// A process's one answer about its installation. Clones share it.
#[derive(Debug, Clone)]
pub struct Authority(Arc<Mutex<Inner>>);

impl Authority {
    /// One that watches nothing and is always current.
    pub fn unwatched() -> Self {
        Authority(Arc::new(Mutex::new(Inner {
            watched: None,
            seen: None,
            installed: None,
            in_flight: 0,
            refused: 0,
        })))
    }

    /// One that judges the build `running` against `installation`.
    pub fn watching(installation: Installation, running: Stamp) -> Self {
        let authority = Self::unwatched();
        authority.watch(installation, running);
        authority
    }

    /// Judge `running` against `installation` from now on.
    pub fn watch(&self, installation: Installation, running: Stamp) {
        self.inner().watched = Some((installation, running));
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Look at the installation and say where this process stands.
    pub fn look(&self) -> Standing {
        let mut inner = self.inner();
        inner.look();
        inner.standing()
    }

    /// Where the last look left this process, without looking again.
    pub fn standing(&self) -> Standing {
        self.inner().standing()
    }

    /// How many permits have been refused so far. A process that stays
    /// up while stale compares this around what a person asked of it, so
    /// a refusal is said on screen whoever swallowed its error.
    pub fn refusals(&self) -> u64 {
        self.inner().refused
    }

    /// What the last look found at the installation, for a process that
    /// reports it.
    pub fn seen(&self) -> Option<Install> {
        self.inner().seen.clone()
    }

    /// Look, then begin work that writes shared durable state or must be
    /// finished before this process is replaced. Refused once a different
    /// build is installed; a permit already out stays good until dropped.
    pub fn permit(&self) -> Result<Permit, Stale> {
        let mut inner = self.inner();
        inner.look();
        if let Some(installed) = inner.installed.clone() {
            inner.refused += 1;
            return Err(Stale { installed });
        }
        inner.in_flight += 1;
        Ok(Permit(self.0.clone()))
    }
}

impl Authority {
    /// Begin settling work this process has already been handed: the
    /// record of a wake that reached its agent. Never refused, since
    /// refusing it would lose a delivery that happened, and held like any
    /// permit, so the process is not replaced under it.
    pub fn settling(&self) -> Permit {
        self.inner().in_flight += 1;
        Permit(self.0.clone())
    }
}

/// The right to write shared durable state, and the mark of work in
/// flight for as long as it is held.
#[derive(Debug)]
pub struct Permit(Arc<Mutex<Inner>>);

impl Drop for Permit {
    fn drop(&mut self) {
        let mut inner = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        inner.in_flight = inner.in_flight.saturating_sub(1);
    }
}

fn armed() -> &'static Authority {
    static PROCESS: OnceLock<Authority> = OnceLock::new();
    PROCESS.get_or_init(Authority::unwatched)
}

/// This process's authority. Unwatched until [`arm`] is called, which
/// only a process entry point does.
pub fn process() -> Authority {
    #[cfg(test)]
    if let Some(standing_in) = tests::STANDS_IN.with(|a| a.borrow().clone()) {
        return standing_in;
    }
    armed().clone()
}

/// Begin a write of shared durable state by this process: the one door
/// every writer of it goes through, shut to a process that has seen a
/// newer clank installed. A one-shot command watches nothing and is
/// never refused. The only way round it is a [`Permit`] already held for
/// a delivery in flight, passed to the writer that settles it.
pub fn writing() -> Result<Permit, Stale> {
    process().permit()
}

/// Watch this process's own installation. For the entry point of a
/// process that can outlive an install, never for library code: a test
/// binary replaced under a running test must not read as an upgrade.
pub fn arm() {
    if let Some(rendezvous) = crate::build_stamp::rendezvous() {
        armed().watch(
            Installation::at(rendezvous.path),
            crate::build_stamp::running(),
        );
    }
}

/// The pause after a failed replacement, doubled up to [`RETRY_CAP`].
const FIRST_RETRY: Duration = Duration::from_millis(500);
const RETRY_CAP: Duration = Duration::from_secs(8);

/// What one pass of a [`Mover`] came to. There is no outcome for a
/// replacement that worked: that process is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pass {
    Current,
    /// Replaceable once what is in flight is finished.
    Deferred,
    /// The replacement was tried and failed; this process goes on as it
    /// was and tries again after the pause.
    Failed {
        why: String,
        retry_in: Duration,
    },
    /// A failed replacement's pause is still running.
    Waiting,
    /// It is stale and stays: replacing it would break what it hosts.
    Held,
}

impl Pass {
    /// Whether this process is known to run a build that is not the
    /// installed one.
    pub fn stale(&self) -> bool {
        !matches!(self, Pass::Current)
    }
}

/// What a process hosts that a replacement would break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hosting {
    Nothing,
    /// A live remote session or tunnel, or a person mid-input: a restart
    /// drops their connection or their words.
    Someone,
}

/// A process that replaces itself with the installed build at its safe
/// point, and keeps trying when that fails.
#[derive(Debug)]
pub struct Mover {
    authority: Authority,
    retry: Option<(Instant, Duration)>,
}

impl Mover {
    pub fn new(authority: Authority) -> Self {
        Mover {
            authority,
            retry: None,
        }
    }

    pub fn authority(&self) -> &Authority {
        &self.authority
    }

    /// Look at the installation, and at the safe point become the build
    /// installed there through `exec`, which returns only if it failed.
    /// The installation is read again on every pass, so a retry becomes
    /// whatever is installed by then.
    pub fn pass(
        &mut self,
        now: Instant,
        hosting: Hosting,
        exec: impl FnOnce(&Build) -> std::io::Error,
    ) -> Pass {
        let installed = match self.authority.look() {
            Standing::Current => {
                self.retry = None;
                return Pass::Current;
            }
            Standing::Quiescing { .. } => return Pass::Deferred,
            Standing::Replaceable { installed } => installed,
        };
        if hosting == Hosting::Someone {
            return Pass::Held;
        }
        if self.retry.is_some_and(|(at, _)| now < at) {
            return Pass::Waiting;
        }
        let why = exec(&installed).to_string();
        let pause = match self.retry {
            Some((_, pause)) => (pause * 2).min(RETRY_CAP),
            None => FIRST_RETRY,
        };
        self.retry = Some((now + pause, pause));
        Pass::Failed {
            why,
            retry_in: pause,
        }
    }

    /// How long until the next pass is due: a beat, or a failed
    /// replacement's pause when that ends sooner.
    pub fn due_in(&self, now: Instant) -> Duration {
        match self.retry {
            Some((at, _)) => BEAT.min(at.saturating_duration_since(now)),
            None => BEAT,
        }
    }
}

/// The runtime's clock, so a retry's pause and the sleep that waits it
/// out are measured on the same one.
fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}

/// A [`Mover`] together with the way its process becomes the installed
/// build.
pub struct Replacing<E> {
    mover: Mover,
    exec: E,
}

impl Replacing<fn(&Build) -> std::io::Error> {
    /// One with nothing to watch, which never replaces anything.
    pub fn never() -> Self {
        Self::new(Authority::unwatched(), |_| {
            std::io::Error::other("this process watches no installation")
        })
    }
}

impl<E: FnMut(&Build) -> std::io::Error> Replacing<E> {
    /// `exec` becomes the installed build and returns only if that failed.
    pub fn new(authority: Authority, exec: E) -> Self {
        Replacing {
            mover: Mover::new(authority),
            exec,
        }
    }

    /// One pass, now. See [`Mover::pass`].
    pub fn pass(&mut self, hosting: Hosting) -> Pass {
        let pass = self.mover.pass(now(), hosting, &mut self.exec);
        if let Pass::Failed { why, retry_in } = &pass {
            // Never to the terminal or a harness: a retried replacement
            // is nobody's news.
            tracing::debug!(%why, ?retry_in, "could not become the installed clank");
        }
        pass
    }

    /// How long until the next pass is due.
    pub fn due_in(&self) -> Duration {
        self.mover.due_in(now())
    }

    /// Whether the last pass found a different build installed.
    pub fn stale(&self) -> bool {
        self.mover.authority().standing().stale().is_some()
    }

    /// Pass on every beat, for as long as this is polled. For a process
    /// to select against while it is parked: whatever it is parked on
    /// goes first, so a pass only ever runs with nothing in hand.
    pub async fn parked(&mut self) -> std::convert::Infallible {
        loop {
            tokio::time::sleep(self.due_in()).await;
            self.pass(Hosting::Nothing);
        }
    }
}

/// Become the installed clank with `args`, on this process's own stdio,
/// pid and environment. Returns only if that failed.
pub fn exec_installed(args: impl IntoIterator<Item = std::ffi::OsString>) -> std::io::Error {
    use crate::proc_run::{Out, Spec, Stdin};
    let Some(rendezvous) = crate::build_stamp::rendezvous() else {
        return std::io::Error::other("this process does not know where clank is installed");
    };
    crate::proc_run::exec(
        Spec::new(&rendezvous.path)
            .args(args)
            .stdin(Stdin::Inherit)
            .stdout(Out::Inherit)
            .stderr(Out::Inherit),
    )
}

/// Become the installed clank with the arguments this process was started
/// with. Returns only if that failed.
pub fn exec_installed_as_started() -> std::io::Error {
    exec_installed(std::env::args_os().skip(1))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    thread_local! {
        pub(super) static STANDS_IN: std::cell::RefCell<Option<Authority>> =
            const { std::cell::RefCell::new(None) };
    }

    /// `authority` as this thread's process authority, until dropped: for
    /// driving real writers and actions as a stale process would.
    pub(crate) struct AsProcess(());

    impl AsProcess {
        pub(crate) fn of(authority: &Authority) -> Self {
            STANDS_IN.with(|a| *a.borrow_mut() = Some(authority.clone()));
            AsProcess(())
        }
    }

    impl Drop for AsProcess {
        fn drop(&mut self) {
            STANDS_IN.with(|a| *a.borrow_mut() = None);
        }
    }

    /// A fixture installation whose build a test replaces.
    pub(crate) struct Installed {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
    }

    impl Installed {
        /// The running build, installed.
        pub(crate) fn current() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("clank");
            let installed = Installed { _dir: dir, path };
            installed.install(crate::build_stamp::running());
            installed
        }

        pub(crate) fn install(&self, stamp: Stamp) {
            let tmp = self.path.with_extension("new");
            std::fs::write(&tmp, crate::build_stamp::executable_with(stamp, "9.9.9")).unwrap();
            std::fs::rename(&tmp, &self.path).unwrap();
        }

        /// Install the `n`th build that is not the running one.
        pub(crate) fn replace(&self, n: u8) -> Stamp {
            let mut other = crate::build_stamp::running().0;
            other[0] ^= n;
            self.install(Stamp(other));
            Stamp(other)
        }

        pub(crate) fn remove(&self) {
            std::fs::remove_file(&self.path).unwrap();
        }

        pub(crate) fn authority(&self) -> Authority {
            Authority::watching(
                Installation::at(self.path.clone()),
                crate::build_stamp::running(),
            )
        }
    }

    fn never(_: &Build) -> std::io::Error {
        panic!("a replacement was tried")
    }

    #[test]
    fn an_unwatched_process_is_always_current() {
        let authority = Authority::unwatched();
        assert_eq!(authority.look(), Standing::Current);
        assert!(authority.permit().is_ok());
        assert_eq!(
            Mover::new(authority).pass(Instant::now(), Hosting::Nothing, never),
            Pass::Current
        );
    }

    /// The same build installed asks for nothing; another one, with
    /// nothing in flight, is replaced once per pass.
    #[test]
    fn a_replaced_build_with_nothing_in_flight_is_replaced_once() {
        let installed = Installed::current();
        let mut mover = Mover::new(installed.authority());
        let now = Instant::now();
        assert_eq!(mover.pass(now, Hosting::Nothing, never), Pass::Current);

        let other = installed.replace(1);
        let tried = std::cell::RefCell::new(Vec::new());
        let pass = mover.pass(now, Hosting::Nothing, |build| {
            tried.borrow_mut().push(build.stamp);
            std::io::Error::other("no exec in a test")
        });
        assert_eq!(
            *tried.borrow(),
            vec![other],
            "asked once, for what is installed"
        );
        assert_eq!(
            pass,
            Pass::Failed {
                why: "no exec in a test".into(),
                retry_in: FIRST_RETRY
            }
        );
    }

    /// Work in flight defers the replacement until it is finished, and a
    /// stale process is given no new permit meanwhile.
    #[test]
    fn work_in_flight_defers_the_replacement_and_no_new_work_begins() {
        let installed = Installed::current();
        let authority = installed.authority();
        let mut mover = Mover::new(authority.clone());
        let presenting = authority.permit().expect("current");
        let other = installed.replace(1);
        let now = Instant::now();

        assert_eq!(mover.pass(now, Hosting::Nothing, never), Pass::Deferred);
        assert!(matches!(
            authority.standing(),
            Standing::Quiescing { in_flight: 1, .. }
        ));
        assert_eq!(
            authority.permit().err().map(|stale| stale.installed.stamp),
            Some(other)
        );

        drop(presenting);
        let tried = std::cell::Cell::new(0);
        mover.pass(now, Hosting::Nothing, |_| {
            tried.set(tried.get() + 1);
            std::io::Error::other("no exec in a test")
        });
        assert_eq!(tried.get(), 1, "replaced once the result is settled");
    }

    /// A failed replacement leaves the process as it was, backs off, and
    /// on its next attempt becomes whatever is installed by then.
    #[test]
    fn a_failed_replacement_is_retried_against_what_is_installed_then() {
        let installed = Installed::current();
        let mut mover = Mover::new(installed.authority());
        installed.replace(1);
        let began = Instant::now();
        let fail = |_: &Build| std::io::Error::other("the binary is missing");

        let mut pauses = Vec::new();
        let mut now = began;
        for _ in 0..6 {
            match mover.pass(now, Hosting::Nothing, fail) {
                Pass::Failed { retry_in, .. } => {
                    assert_eq!(
                        mover.pass(
                            now + retry_in - Duration::from_millis(1),
                            Hosting::Nothing,
                            never
                        ),
                        Pass::Waiting
                    );
                    assert_eq!(mover.due_in(now), BEAT.min(retry_in));
                    pauses.push(retry_in);
                    now += retry_in;
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(
            pauses,
            [500, 1000, 2000, 4000, 8000, 8000].map(Duration::from_millis)
        );

        // Mid-install the binary is gone: still stale, still retried.
        installed.remove();
        assert!(matches!(
            mover.pass(now + RETRY_CAP, Hosting::Nothing, fail),
            Pass::Failed { .. }
        ));

        let third = installed.replace(2);
        let tried = std::cell::RefCell::new(None);
        mover.pass(now + RETRY_CAP * 3, Hosting::Nothing, |build| {
            *tried.borrow_mut() = Some(build.stamp);
            std::io::Error::other("no exec in a test")
        });
        assert_eq!(*tried.borrow(), Some(third), "the build installed since");

        installed.install(crate::build_stamp::running());
        assert_eq!(
            mover.pass(now + RETRY_CAP * 6, Hosting::Nothing, never),
            Pass::Current,
            "its own build installed again"
        );
        assert_eq!(mover.due_in(now), BEAT);
    }

    /// A process hosting someone is never replaced, and is stale all the
    /// same: it is refused every write.
    #[test]
    fn a_process_hosting_someone_stays_and_stops_writing() {
        let installed = Installed::current();
        let authority = installed.authority();
        let mut mover = Mover::new(authority.clone());
        installed.replace(1);
        let pass = mover.pass(Instant::now(), Hosting::Someone, never);
        assert_eq!(pass, Pass::Held);
        assert!(pass.stale());
        assert!(authority.permit().is_err());
    }

    /// Every kind of shared durable state sits behind the one authority:
    /// with a newer clank installed none of them is written, and with
    /// this build installed again each of them is.
    #[test]
    fn a_stale_process_writes_no_shared_state_of_any_kind() {
        use crate::cli::github::log::{EventLog, Transport};
        let installed = Installed::current();
        let authority = installed.authority();
        let repo = crate::cli::monitor::TempRepo::new();
        let agent = tempfile::tempdir().unwrap();
        let turn: clank_core::turn_policy::Persisted =
            serde_json::from_str(include_str!("../tests/fixtures/formats/turn.json")).unwrap();
        let state: crate::cli::agent_state::AgentState =
            serde_json::from_str(include_str!("../tests/fixtures/formats/state.json")).unwrap();
        let account = crate::cli::source_status::Account {
            monitor: std::process::id() as i32,
            birth: crate::proc_identity::token_for(std::process::id() as i32).unwrap(),
            sources: Vec::new(),
        };
        let item = clank_core::wait::WaitItem::Message {
            presentation: None,
            requires_reply: false,
            reply_route: None,
            handle: "1".into(),
            from: None,
            repo: None,
            subject: "a message".into(),
            body: "to an agent".into(),
            selector: None,
        };
        crate::cli::monitor::ensure_runtime_dir().unwrap();
        let events = agent.path().join("events");
        let written = || {
            [
                ("turn state", agent.path().join("turn.json")),
                ("agent state", agent.path().join("state.json")),
                (
                    "source account",
                    crate::cli::source_status::path(repo.path()),
                ),
                ("event log", events.join("messages.jsonl")),
            ]
            .into_iter()
            .filter(|(_, path)| path.exists())
            .map(|(kind, _)| kind)
            .collect::<Vec<_>>()
        };
        let write_each = || -> Vec<bool> {
            let permitted = || authority.permit().map_err(std::io::Error::from);
            vec![
                permitted()
                    .and_then(|p| crate::cli::turn_custody::write_stored(agent.path(), &turn, &p))
                    .is_ok(),
                permitted()
                    .map(|p| crate::cli::agent_state::store(agent.path(), &state, &p))
                    .is_ok(),
                permitted()
                    .and_then(|p| crate::cli::source_status::publish(repo.path(), &account, &p))
                    .is_ok(),
                EventLog::open(&events, "messages")
                    .unwrap()
                    .guarded_by(authority.clone())
                    .append_inbox(Transport::Message, None, None, None, &item)
                    .is_ok(),
            ]
        };

        installed.replace(1);
        assert_eq!(write_each(), vec![false; 4]);
        assert_eq!(written(), Vec::<&str>::new());

        installed.install(crate::build_stamp::running());
        assert_eq!(write_each(), vec![true; 4]);
        assert_eq!(
            written(),
            vec!["turn state", "agent state", "source account", "event log"]
        );
        let _ = std::fs::remove_file(crate::cli::source_status::path(repo.path()));
    }

    /// The families a process writes through its own authority, driven as
    /// a stale process would drive them: an agent's ledger, recovery's
    /// issued number, the user's config, an agent's config, the repo's
    /// roster, a git mutation. None is made, each says why, and on its own build again
    /// each is.
    #[test]
    fn a_stale_process_begins_no_write_through_any_helper() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        let home = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let agent = repo.path().join(".clank/agents/claude");
        std::fs::create_dir_all(&agent).unwrap();
        let label = clank_core::AgentLabel::parse("claude").unwrap();
        let installed = Installed::current();
        let authority = installed.authority();
        let _as = AsProcess::of(&authority);
        let each = || -> Vec<(&'static str, Result<(), String>)> {
            let said = |e: anyhow::Error| format!("{e:#}");
            vec![
                (
                    "ledger",
                    crate::cli::ledger::store(&agent, &Default::default())
                        .map_err(|e| said(e.into())),
                ),
                (
                    "recovery's issued number",
                    crate::cli::recovery::store_issued(&agent, 3).map_err(|e| said(e.into())),
                ),
                (
                    "user config",
                    crate::cli::team::transact_user_config(home.path(), |doc| {
                        doc.insert("probe".into(), true.into());
                        Ok(())
                    })
                    .map_err(said),
                ),
                (
                    "agent config",
                    crate::agent_store::set_auto_mode(
                        repo.path(),
                        &label,
                        clank_core::AutoMode::Off,
                    )
                    .map_err(said),
                ),
                (
                    "roster",
                    crate::cli::agent::set_repo_master(repo.path(), &label).map_err(said),
                ),
                (
                    "git",
                    crate::git_plumbing::init_repo(scratch.path()).map_err(said),
                ),
                (
                    "git object",
                    crate::git_plumbing::empty_tree(scratch.path())
                        .map(drop)
                        .map_err(said),
                ),
                (
                    "git index",
                    crate::git_plumbing::write_index_tree(scratch.path())
                        .map(drop)
                        .map_err(said),
                ),
            ]
        };
        let tree = |root: &std::path::Path| {
            let mut files = Vec::new();
            let mut dirs = vec![root.to_path_buf()];
            while let Some(dir) = dirs.pop() {
                for entry in std::fs::read_dir(dir).unwrap().flatten() {
                    match entry.path().is_dir() {
                        true => dirs.push(entry.path()),
                        false if entry.path().extension().is_some_and(|x| x == "lock") => {}
                        false => files.push(entry.path()),
                    }
                }
            }
            files.sort();
            files
        };

        installed.replace(1);
        for (what, tried) in each() {
            let why = tried.expect_err(what);
            assert!(why.contains(STALE_BADGE), "{what}: {why}");
        }
        assert_eq!(tree(repo.path()), Vec::<std::path::PathBuf>::new());
        assert_eq!(tree(home.path()), Vec::<std::path::PathBuf>::new());
        assert!(
            !scratch.path().join(".git").exists(),
            "a stale process ran git"
        );

        installed.install(crate::build_stamp::running());
        for (what, tried) in each() {
            // The roster refuses an agent it does not list, which is its
            // own rule and not the authority's.
            if let Err(why) = tried {
                assert!(
                    what == "roster" && !why.contains(STALE_BADGE),
                    "{what}: {why}"
                );
            }
        }
        let written = tree(repo.path());
        for name in ["config.json", "recovery-issued.json"] {
            assert!(written.contains(&agent.join(name)), "{name}: {written:?}");
        }
        assert_eq!(tree(home.path()).len(), 1);
        assert!(scratch.path().join(".git").exists());
    }

    /// The record of a delivery that already happened is never refused,
    /// and holds off the replacement like any work in flight.
    #[test]
    fn settling_a_delivery_is_granted_to_a_stale_process_and_defers_it() {
        let installed = Installed::current();
        let authority = installed.authority();
        let mut mover = Mover::new(authority.clone());
        installed.replace(1);
        assert!(authority.permit().is_err());
        let settling = authority.settling();
        assert_eq!(
            mover.pass(Instant::now(), Hosting::Nothing, never),
            Pass::Deferred
        );
        drop(settling);
        assert!(matches!(authority.standing(), Standing::Replaceable { .. }));
    }

    /// An installation that cannot be read proves nothing either way.
    #[test]
    fn an_unreadable_installation_changes_no_standing() {
        let installed = Installed::current();
        let authority = installed.authority();
        installed.remove();
        assert_eq!(authority.look(), Standing::Current);
        assert!(authority.permit().is_ok());

        installed.replace(1);
        assert!(authority.look().stale().is_some());
        installed.remove();
        assert!(authority.look().stale().is_some(), "still replaced");
        assert!(matches!(authority.seen(), Some(Install::Unavailable(_))));
    }
}
