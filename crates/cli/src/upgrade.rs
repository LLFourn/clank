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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use crate::build_stamp::{Build, Install, Installation, Stamp};

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

type Wake = Arc<dyn Fn() + Send + Sync>;

struct Inner {
    watched: Option<(Installation, Stamp)>,
    seen: Option<Install>,
    /// The latest proof of replacement. An installation that cannot be
    /// read proves nothing, so it neither sets nor clears this.
    installed: Option<Build>,
    in_flight: usize,
    refused: u64,
    /// Changes heard at the installation by any subscription's watch.
    heard: Arc<AtomicU64>,
    subscribers: Vec<(u64, Wake)>,
    next_subscriber: u64,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("watched", &self.watched)
            .field("seen", &self.seen)
            .field("installed", &self.installed)
            .field("in_flight", &self.in_flight)
            .field("refused", &self.refused)
            .field("subscribers", &self.subscribers.len())
            .finish()
    }
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
            heard: Arc::default(),
            subscribers: Vec::new(),
            next_subscriber: 0,
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

    /// `wake`, counting each change at the installation as heard.
    fn hearing(&self, wake: Wake) -> Wake {
        let heard = self.inner().heard.clone();
        Arc::new(move || {
            heard.fetch_add(1, Ordering::AcqRel);
            wake()
        })
    }

    /// How many changes at the installation this process's watches have
    /// heard. A failed replacement is tried again only once it moves.
    pub fn installs_heard(&self) -> u64 {
        self.inner().heard.load(Ordering::Acquire)
    }

    /// Call `moved` whenever where this process stands may have moved: an
    /// entry of a directory holding the installation changed (an install
    /// replaces the file, or retargets the link to it), the file itself was
    /// written, or the last work in flight on a stale build finished.
    /// `moved` only signals; the subscriber looks. Subscribe before
    /// looking, so an install landing between the two is still heard.
    ///
    /// `failed` is told when the watch fails once armed, after which
    /// nothing more is heard. A watch that cannot be armed, or covers less
    /// than every directory an install changes, is an error rather than a
    /// subscription that never wakes. `Ok(None)` when no installation is
    /// watched, so this process is never replaced. Ends when the
    /// [`Subscription`] is dropped.
    pub fn subscribe(
        &self,
        moved: impl Fn() + Send + Sync + 'static,
        failed: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Option<Subscription>, String> {
        let wake: Wake = Arc::new(moved);
        let Some(installed) = self
            .inner()
            .watched
            .as_ref()
            .map(|(installation, _)| installation.path().to_path_buf())
        else {
            return Ok(None);
        };
        if install_dirs(&installed).is_empty() {
            return Err(format!(
                "no directory holding {} exists to watch",
                installed.display()
            ));
        }
        let (watch, missed) = install_watcher(&installed, self.hearing(wake.clone()), failed)?;
        if let Some((dir, e)) = missed.into_iter().next() {
            return Err(format!("{}: {e}", dir.display()));
        }
        Ok(Some(self.subscribed(wake, Some(watch))))
    }

    fn subscribed(
        &self,
        wake: Wake,
        watch: Option<crate::proc_identity::tree_watch::KernelWatcher>,
    ) -> Subscription {
        let mut inner = self.inner();
        let id = inner.next_subscriber;
        inner.next_subscriber += 1;
        inner.subscribers.push((id, wake));
        Subscription {
            id,
            authority: self.0.clone(),
            _watch: watch,
        }
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
        if inner.in_flight > 0 || inner.installed.is_none() {
            return;
        }
        let wakes: Vec<Wake> = inner.subscribers.iter().map(|(_, w)| w.clone()).collect();
        drop(inner);
        for wake in wakes {
            wake();
        }
    }
}

/// A live [`Authority::subscribe`]. Dropping it stops the watching.
pub struct Subscription {
    id: u64,
    authority: Arc<Mutex<Inner>>,
    _watch: Option<crate::proc_identity::tree_watch::KernelWatcher>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut inner = self
            .authority
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        inner.subscribers.retain(|(id, _)| *id != self.id);
    }
}

/// Watch an installation: the directories holding its path and the file
/// it resolves to, for an install that renames over it or retargets a
/// link, and that file's own writes, for one that writes over it in
/// place. `failed` is told of a watch that fails once armed; the
/// directories it could not watch are returned.
fn install_watcher(
    installed: &std::path::Path,
    wake: Wake,
    failed: impl Fn(String) + Send + 'static,
) -> Result<
    (
        crate::proc_identity::tree_watch::KernelWatcher,
        Vec<(std::path::PathBuf, notify::Error)>,
    ),
    String,
> {
    use notify::Watcher as _;
    let path = installed.to_path_buf();
    let mut watcher = crate::proc_identity::tree_watch::KernelWatcher::new(
        move |file| file == path || std::fs::canonicalize(&path).is_ok_and(|real| real == file),
        move |res: notify::Result<notify::Event>| match crate::fs_notice::notice(&res) {
            crate::fs_notice::Notice::Failed(why) => failed(why),
            crate::fs_notice::Notice::Read => {}
            _ => wake(),
        },
    )
    .map_err(|e| e.to_string())?;
    let missed = install_dirs(installed)
        .into_iter()
        .filter_map(|dir| {
            let missed = watcher
                .watch(&dir, notify::RecursiveMode::NonRecursive)
                .err();
            missed.map(|e| (dir, e))
        })
        .collect();
    Ok((watcher, missed))
}

/// The directories whose entries an install changes: the one holding
/// the path clank is installed at, and the one holding what that path
/// resolves to when it is a link.
fn install_dirs(path: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> =
        [Some(path.to_path_buf()), std::fs::canonicalize(path).ok()]
            .into_iter()
            .flatten()
            .filter_map(|p| p.parent().map(std::path::Path::to_path_buf))
            .filter(|dir| dir.is_dir())
            .collect();
    dirs.dedup();
    dirs
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

/// What one pass of a [`Mover`] came to. There is no outcome for a
/// replacement that worked: that process is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pass {
    Current,
    /// Replaceable once what is in flight is finished.
    Deferred,
    /// The replacement was tried and failed; this process goes on as it
    /// was until the installation changes.
    Failed {
        why: String,
    },
    /// A replacement failed and nothing has changed at the installation
    /// since, so it is not tried again.
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
/// point, and tries again when a replacement failed only once the
/// installation has changed.
#[derive(Debug)]
pub struct Mover {
    authority: Authority,
    failed: Option<Failure>,
}

/// A replacement that failed: the build it was to become, and the
/// installs heard before it was tried.
#[derive(Debug)]
struct Failure {
    build: Build,
    heard: u64,
}

impl Mover {
    pub fn new(authority: Authority) -> Self {
        Mover {
            authority,
            failed: None,
        }
    }

    pub fn authority(&self) -> &Authority {
        &self.authority
    }

    /// Look at the installation, and at the safe point become the build
    /// installed there through `exec`, which returns only if it failed.
    /// After a failure nothing is tried until another build is installed
    /// or a watch hears the installation change, so a pass on any other
    /// wake costs no attempt.
    pub fn pass(&mut self, hosting: Hosting, exec: impl FnOnce(&Build) -> std::io::Error) -> Pass {
        // Read before the look: a change landing between the two is then
        // counted as after the attempt, and earns one more.
        let heard = self.authority.installs_heard();
        let installed = match self.authority.look() {
            Standing::Current => {
                self.failed = None;
                return Pass::Current;
            }
            Standing::Quiescing { .. } => return Pass::Deferred,
            Standing::Replaceable { installed } => installed,
        };
        if hosting == Hosting::Someone {
            return Pass::Held;
        }
        if self
            .failed
            .as_ref()
            .is_some_and(|failed| failed.build == installed && failed.heard == heard)
        {
            return Pass::Waiting;
        }
        let why = exec(&installed).to_string();
        self.failed = Some(Failure {
            build: installed,
            heard,
        });
        Pass::Failed { why }
    }

    /// A watch on the installation was armed after a gap in which changes
    /// went unheard: a failed replacement is tried once more.
    pub fn rearmed(&mut self) {
        self.failed = None;
    }
}

/// A [`Mover`] together with the way its process becomes the installed
/// build.
pub struct Replacing<E> {
    mover: Mover,
    exec: E,
    hear: Hear,
    /// The watch on the installation, kept from the first park until it
    /// fails, so no change between two parks goes unheard.
    hearing: Option<Hearing>,
}

struct Hearing {
    _watch: Option<Box<dyn Send>>,
    woken: Arc<tokio::sync::Notify>,
    deaf: Arc<Mutex<Option<String>>>,
}

/// How a [`Replacing`] hears its installation: what wakes it, what tells
/// it the watch failed, and the guard that keeps the watch, `None` for a
/// process that watches no installation. A seam, so a test can fail it.
pub(crate) type Hear = fn(
    &Authority,
    Box<dyn Fn() + Send + Sync>,
    Box<dyn Fn(String) + Send + Sync>,
) -> Result<Option<Box<dyn Send>>, String>;

fn hear_installs(
    authority: &Authority,
    moved: Box<dyn Fn() + Send + Sync>,
    failed: Box<dyn Fn(String) + Send + Sync>,
) -> Result<Option<Box<dyn Send>>, String> {
    Ok(authority
        .subscribe(moved, failed)?
        .map(|subscription| Box::new(subscription) as Box<dyn Send>))
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
            hear: hear_installs,
            hearing: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn hearing_through(mut self, hear: Hear) -> Self {
        self.hear = hear;
        self
    }

    /// One pass, now. See [`Mover::pass`].
    pub fn pass(&mut self, hosting: Hosting) -> Pass {
        let pass = self.mover.pass(hosting, &mut self.exec);
        if let Pass::Failed { why } = &pass {
            // Never to the terminal or a harness: the stale badge already
            // says this process is not the installed build.
            tracing::debug!(%why, "could not become the installed clank");
        }
        pass
    }

    pub fn authority(&self) -> &Authority {
        self.mover.authority()
    }

    /// Whether the last pass found a different build installed.
    pub fn stale(&self) -> bool {
        self.mover.authority().standing().stale().is_some()
    }

    /// Pass now, and again each time the installation may have moved, for
    /// as long as this is polled. For a process to select against while
    /// it is parked: whatever it is parked on goes first, so a pass only
    /// ever runs with nothing in hand. Ends only when the installation
    /// can no longer be heard, saying why: from then on this process is
    /// not replaced, and goes on as it is.
    pub async fn parked(&mut self) -> String {
        match self.pass_until(|_| false).await {
            Ok(()) => unreachable!("a pass that is never done"),
            Err(deaf) => deaf,
        }
    }

    /// Pass as [`Self::parked`] does until a pass finds this process
    /// current, which may be at once, or why the installation can no
    /// longer be heard.
    pub async fn until_current(&mut self) -> Result<(), String> {
        self.pass_until(|pass| *pass == Pass::Current).await
    }

    async fn pass_until(&mut self, done: impl Fn(&Pass) -> bool) -> Result<(), String> {
        let (woken, deaf) = match &self.hearing {
            Some(hearing) => (hearing.woken.clone(), hearing.deaf.clone()),
            None => {
                let woken = Arc::new(tokio::sync::Notify::new());
                let deaf = Arc::new(Mutex::new(None::<String>));
                let watch = (self.hear)(
                    self.authority(),
                    Box::new({
                        let woken = woken.clone();
                        move || woken.notify_one()
                    }),
                    Box::new({
                        let (woken, deaf) = (woken.clone(), deaf.clone());
                        move |why| {
                            deaf.lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .get_or_insert(why);
                            woken.notify_one();
                        }
                    }),
                )
                .map_err(|why| format!("cannot watch the installation: {why}"))?;
                self.hearing = Some(Hearing {
                    _watch: watch,
                    woken: woken.clone(),
                    deaf: deaf.clone(),
                });
                self.mover.rearmed();
                (woken, deaf)
            }
        };
        loop {
            if done(&self.pass(Hosting::Nothing)) {
                return Ok(());
            }
            woken.notified().await;
            if let Some(why) = deaf.lock().unwrap_or_else(PoisonError::into_inner).take() {
                self.hearing = None;
                return Err(format!("the watch on the installation failed: {why}"));
            }
        }
    }

    /// A watch on the installation was armed again after one was lost:
    /// what changed between is unheard, so a failed replacement is tried
    /// once more.
    pub fn rearmed(&mut self) {
        self.mover.rearmed();
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
    use std::time::Duration;

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

        /// Install the `n`th other build by writing over the file in
        /// place, as a copy onto the same path does: no entry of its
        /// directory changes.
        pub(crate) fn overwrite(&self, n: u8) -> Stamp {
            use std::io::Write as _;
            let mut other = crate::build_stamp::running().0;
            other[0] ^= n;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&self.path)
                .unwrap();
            file.write_all(&crate::build_stamp::executable_with(Stamp(other), "9.9.9"))
                .unwrap();
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
            Mover::new(authority).pass(Hosting::Nothing, never),
            Pass::Current
        );
    }

    /// The same build installed asks for nothing; another one, with
    /// nothing in flight, is replaced once per pass.
    #[test]
    fn a_replaced_build_with_nothing_in_flight_is_replaced_once() {
        let installed = Installed::current();
        let mut mover = Mover::new(installed.authority());
        assert_eq!(mover.pass(Hosting::Nothing, never), Pass::Current);

        let other = installed.replace(1);
        let tried = std::cell::RefCell::new(Vec::new());
        let pass = mover.pass(Hosting::Nothing, |build| {
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

        assert_eq!(mover.pass(Hosting::Nothing, never), Pass::Deferred);
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
        mover.pass(Hosting::Nothing, |_| {
            tried.set(tried.get() + 1);
            std::io::Error::other("no exec in a test")
        });
        assert_eq!(tried.get(), 1, "replaced once the result is settled");
    }

    /// A failed replacement leaves the process as it was and is not
    /// tried again on any pass, however many, until the installation
    /// changes: another build installed, or a change a watch heard. Then
    /// it becomes whatever is installed by then.
    #[test]
    fn a_failed_replacement_waits_for_the_installation_to_change() {
        let installed = Installed::current();
        let authority = installed.authority();
        let mut mover = Mover::new(authority.clone());
        installed.replace(1);
        let fail = |_: &Build| std::io::Error::other("the binary is missing");

        assert!(matches!(
            mover.pass(Hosting::Nothing, fail),
            Pass::Failed { .. }
        ));
        for _ in 0..100 {
            assert_eq!(mover.pass(Hosting::Nothing, never), Pass::Waiting);
        }

        let third = installed.replace(2);
        let tried = std::cell::RefCell::new(None);
        mover.pass(Hosting::Nothing, |build| {
            *tried.borrow_mut() = Some(build.stamp);
            std::io::Error::other("no exec in a test")
        });
        assert_eq!(*tried.borrow(), Some(third), "the build installed since");
        assert_eq!(mover.pass(Hosting::Nothing, never), Pass::Waiting);

        authority.inner().heard.fetch_add(1, Ordering::AcqRel);
        let tried = std::cell::Cell::new(0);
        mover.pass(Hosting::Nothing, |_| {
            tried.set(tried.get() + 1);
            std::io::Error::other("no exec in a test")
        });
        assert_eq!(tried.get(), 1, "the same build, after a change was heard");

        installed.install(crate::build_stamp::running());
        assert_eq!(
            mover.pass(Hosting::Nothing, never),
            Pass::Current,
            "its own build installed again"
        );
    }

    /// A watch armed again after one was lost may have missed a repair of
    /// the same build: the failed replacement is tried once more, and
    /// then waits again.
    #[test]
    fn a_rearmed_watch_tries_a_failed_replacement_once_more() {
        let installed = Installed::current();
        let mut mover = Mover::new(installed.authority());
        installed.replace(1);
        let tries = std::cell::Cell::new(0);
        let fail = |_: &Build| {
            tries.set(tries.get() + 1);
            std::io::Error::other("not executable")
        };
        assert!(matches!(
            mover.pass(Hosting::Nothing, fail),
            Pass::Failed { .. }
        ));
        assert_eq!(mover.pass(Hosting::Nothing, fail), Pass::Waiting);
        mover.rearmed();
        assert!(matches!(
            mover.pass(Hosting::Nothing, fail),
            Pass::Failed { .. }
        ));
        assert_eq!(mover.pass(Hosting::Nothing, fail), Pass::Waiting);
        assert_eq!(tries.get(), 2);
    }

    /// What a subscription's watch hears at the installation is counted,
    /// and the last permit's drop, which moves no installation, is not.
    #[test]
    fn only_a_change_at_the_installation_is_heard() {
        let installed = Installed::current();
        let authority = installed.authority();
        let presenting = authority.permit().expect("current");
        let (tx, rx) = std::sync::mpsc::channel();
        let _subscription = authority
            .subscribe(
                move || {
                    let _ = tx.send(());
                },
                |why| panic!("{why}"),
            )
            .unwrap();
        assert_eq!(authority.installs_heard(), 0);
        installed.replace(1);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the install");
        std::thread::sleep(Duration::from_millis(200));
        while rx.try_recv().is_ok() {}
        let heard = authority.installs_heard();
        assert!(heard > 0);
        let _ = authority.look();
        drop(presenting);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the last permit");
        assert_eq!(authority.installs_heard(), heard);
    }

    /// A process hosting someone is never replaced, and is stale all the
    /// same: it is refused every write.
    #[test]
    fn a_process_hosting_someone_stays_and_stops_writing() {
        let installed = Installed::current();
        let authority = installed.authority();
        let mut mover = Mover::new(authority.clone());
        installed.replace(1);
        let pass = mover.pass(Hosting::Someone, never);
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
        crate::agent_store::state::tests::bind(
            agent.path(),
            clank_core::Tool::Claude,
            turn.binding.as_str(),
        );
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/formats/state.json")).unwrap();
        let state: crate::cli::agent_state::AgentState =
            serde_json::from_value(fixture["bound"]["projection"].clone()).unwrap();
        let bound = |part: &str| {
            crate::agent_store::state::tests::raw(agent.path())
                .pointer(&format!("/bound/{part}"))
                .is_some()
        };
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
                ("turn state", bound("turn")),
                ("agent state", bound("projection")),
                (
                    "source account",
                    crate::cli::source_status::path(repo.path()).exists(),
                ),
                ("event log", events.join("messages.jsonl").exists()),
            ]
            .into_iter()
            .filter(|(_, written)| *written)
            .map(|(kind, _)| kind)
            .collect::<Vec<_>>()
        };
        let write_each = || -> Vec<bool> {
            let permitted = || authority.permit().map_err(std::io::Error::from);
            vec![
                permitted().is_ok_and(|p| {
                    crate::cli::turn_custody::write_stored(agent.path(), &turn, &p).is_ok()
                }),
                permitted().is_ok_and(|p| {
                    crate::cli::agent_state::store(agent.path(), &turn.binding, &state, &p)
                }),
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
                        .map(drop)
                        .map_err(said),
                ),
                (
                    "recovery's issued number",
                    crate::cli::recovery::store_issued(&agent, 3).map_err(said),
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
        for name in ["config.json", "state.json"] {
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
        assert_eq!(mover.pass(Hosting::Nothing, never), Pass::Deferred);
        drop(settling);
        assert!(matches!(authority.standing(), Standing::Replaceable { .. }));
    }

    /// An install is an event: replacing the file, as `cargo install`
    /// does, wakes a subscriber with no clock involved.
    #[test]
    fn an_install_wakes_a_subscriber() {
        let installed = Installed::current();
        let authority = installed.authority();
        let (tx, rx) = std::sync::mpsc::channel();
        let subscription = authority
            .subscribe(
                move || {
                    let _ = tx.send(());
                },
                |why| panic!("{why}"),
            )
            .unwrap();
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "nothing yet"
        );
        installed.replace(1);
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the install"
        );
        drop(subscription);
        while rx.try_recv().is_ok() {}
        installed.replace(2);
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "nothing once dropped"
        );
    }

    /// The last work in flight on a stale build finishing is the other
    /// thing that moves where it stands.
    #[test]
    fn the_last_permit_dropped_on_a_stale_build_wakes_a_subscriber() {
        let installed = Installed::current();
        let authority = installed.authority();
        let presenting = authority.permit().expect("current");
        installed.replace(1);
        assert!(matches!(authority.look(), Standing::Quiescing { .. }));
        let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _subscription = authority
            .subscribe(
                {
                    let woken = woken.clone();
                    move || {
                        woken.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    }
                },
                |why| panic!("{why}"),
            )
            .unwrap();
        let before = woken.load(std::sync::atomic::Ordering::Acquire);
        drop(presenting);
        assert!(woken.load(std::sync::atomic::Ordering::Acquire) > before);
    }

    /// A parked process passes once when it parks, then only when woken:
    /// an install while parked replaces it, and one that landed before
    /// it subscribed is acted on at once.
    #[tokio::test]
    async fn a_parked_process_is_replaced_by_an_install_and_by_nothing_else() {
        let installed = Installed::current();
        let tried = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut replacing = Replacing::new(installed.authority(), {
            let tried = tried.clone();
            move |build: &Build| {
                tried.lock().unwrap().push(build.stamp);
                std::io::Error::other("no exec in a test")
            }
        });
        let idle = tokio::time::timeout(Duration::from_millis(500), replacing.parked()).await;
        assert!(idle.is_err());
        assert!(tried.lock().unwrap().is_empty(), "nothing to replace");

        let installing = std::thread::spawn({
            let path = installed.path.clone();
            move || {
                std::thread::sleep(Duration::from_millis(200));
                let mut other = crate::build_stamp::running().0;
                other[0] ^= 1;
                let tmp = path.with_extension("new");
                std::fs::write(&tmp, crate::build_stamp::executable_with(Stamp(other), "9"))
                    .unwrap();
                std::fs::rename(&tmp, &path).unwrap();
                Stamp(other)
            }
        });
        let _ = tokio::time::timeout(Duration::from_millis(1500), replacing.parked()).await;
        let other = installing.join().unwrap();
        assert_eq!(tried.lock().unwrap().first(), Some(&other), "while parked");

        let third = installed.replace(2);
        let mut late = Replacing::new(installed.authority(), {
            let tried = tried.clone();
            move |build: &Build| {
                tried.lock().unwrap().push(build.stamp);
                std::io::Error::other("no exec in a test")
            }
        });
        tried.lock().unwrap().clear();
        let _ = tokio::time::timeout(Duration::from_millis(100), late.parked()).await;
        assert_eq!(
            tried.lock().unwrap().first(),
            Some(&third),
            "an install before the subscription"
        );
    }

    /// An install that writes over the installed file in place changes
    /// no directory entry; a parked process still hears it, and is
    /// replaced by the build written.
    #[tokio::test]
    async fn a_parked_process_is_replaced_by_an_install_written_in_place() {
        let installed = Installed::current();
        let path = installed.path.clone();
        let ino = || std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&path).unwrap());
        let before = ino();
        let tried = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut replacing = Replacing::new(installed.authority(), {
            let tried = tried.clone();
            move |build: &Build| {
                tried.lock().unwrap().push(build.stamp);
                std::io::Error::other("no exec in a test")
            }
        });
        let idle = tokio::time::timeout(Duration::from_millis(300), replacing.parked()).await;
        assert!(idle.is_err());
        let installed = std::sync::Arc::new(installed);
        let writing = std::thread::spawn({
            let installed = installed.clone();
            move || {
                std::thread::sleep(Duration::from_millis(200));
                installed.overwrite(1)
            }
        });
        let _ = tokio::time::timeout(Duration::from_millis(1500), replacing.parked()).await;
        let other = writing.join().unwrap();
        assert_eq!(ino(), before, "written in place, not replaced");
        assert_eq!(tried.lock().unwrap().first(), Some(&other), "while parked");
    }

    /// A stale process waiting to become current again returns as soon
    /// as its own build is installed back, and a replacement that failed
    /// meanwhile is not tried again while nothing at the installation
    /// changes.
    #[tokio::test]
    async fn until_current_returns_when_its_build_is_back() {
        let installed = Installed::current();
        installed.replace(1);
        let tries = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut replacing = Replacing::new(installed.authority(), {
            let tries = tries.clone();
            move |_: &Build| {
                tries.fetch_add(1, Ordering::AcqRel);
                std::io::Error::other("no exec in a test")
            }
        });
        let quiet =
            tokio::time::timeout(Duration::from_millis(1500), replacing.until_current()).await;
        assert!(quiet.is_err(), "still stale");
        assert_eq!(tries.load(Ordering::Acquire), 1, "tried once, then waited");
        let restoring = std::thread::spawn({
            let path = installed.path.clone();
            move || {
                std::thread::sleep(Duration::from_millis(200));
                let tmp = path.with_extension("new");
                std::fs::write(
                    &tmp,
                    crate::build_stamp::executable_with(crate::build_stamp::running(), "9"),
                )
                .unwrap();
                std::fs::rename(&tmp, &path).unwrap();
            }
        });
        let back = tokio::time::timeout(Duration::from_secs(5), replacing.until_current())
            .await
            .expect("current again");
        assert_eq!(back, Ok(()));
        restoring.join().unwrap();
    }

    /// An installation that cannot be watched, for want of a directory to
    /// watch or because its watch would not arm, ends a park at once with
    /// why, and so does a watch that fails once armed: none is taken for a
    /// quiet one. A failed replacement waiting on it ends the same way.
    #[tokio::test(start_paused = true)]
    async fn an_installation_that_cannot_be_heard_ends_the_park() {
        let gone = tempfile::tempdir().unwrap();
        let mut unwatchable = Replacing::new(
            Authority::watching(
                Installation::at(gone.path().join("no-such-dir/clank")),
                crate::build_stamp::running(),
            ),
            never,
        );
        let deaf = tokio::time::timeout(Duration::from_secs(60), unwatchable.parked())
            .await
            .expect("it ends");
        assert!(deaf.contains("no directory holding"), "{deaf}");

        let installed = Installed::current();
        let mut refused = Replacing::new(installed.authority(), never)
            .hearing_through(|_, _, _| Err("no descriptors left".into()));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(60), refused.parked())
                .await
                .expect("it ends"),
            "cannot watch the installation: no descriptors left"
        );

        type Failed = Box<dyn Fn(String) + Send + Sync>;
        static FAIL: Mutex<Option<Failed>> = Mutex::new(None);
        installed.replace(1);
        let mut failing = Replacing::new(installed.authority(), |_: &Build| {
            std::io::Error::other("no exec in a test")
        })
        .hearing_through(|_, _, failed| {
            *FAIL.lock().unwrap() = Some(failed);
            Ok(Some(Box::new(())))
        });
        {
            let waiting = failing.until_current();
            tokio::pin!(waiting);
            assert!(
                tokio::time::timeout(Duration::from_secs(60), &mut waiting)
                    .await
                    .is_err(),
                "a failed replacement waits on a live watch"
            );
            (FAIL.lock().unwrap().take().expect("armed"))("the stream died".into());
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(60), waiting)
                    .await
                    .expect("it ends"),
                Err("the watch on the installation failed: the stream died".into())
            );
        }
        let again = failing.until_current();
        tokio::pin!(again);
        assert!(
            tokio::time::timeout(Duration::from_secs(60), &mut again)
                .await
                .is_err()
        );
        assert!(
            FAIL.lock().unwrap().is_some(),
            "the next park arms a watch of its own"
        );
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
