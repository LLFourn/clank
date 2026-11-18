//! A client's time without a monitor (I10): why each attempt failed, typed
//! by what can end it; the events that can; and the one deadline by which
//! an outage is reported when none of them comes.
//!
//! Trying again and reporting are separate. A client attempts again only
//! when one of its failure's [`Source`]s moves, never on a clock. Its
//! report has a single deadline per outage, which neither those events nor
//! the attempts they cause postpone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{Attempt, Conn, OUTAGE_BUDGET, RETIREMENT_BUDGET, outgoing};

/// Why a waiter that must have a monitor has none this time, by what can
/// end it: the [`Source`]s it listens to before it attempts again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unserved {
    /// A monitor still holds the repo and is known to be going: the
    /// ordinary course of an upgrade. Ended by its lease's release or a
    /// monitor publishing; reported after [`RETIREMENT_BUDGET`].
    Retiring(String),
    /// Another client holds the repair permit, starting one. Ended by the
    /// permit's release or the monitor it starts publishing.
    Repairing(String),
    /// A newer build's monitor refused this one. Ended by an install,
    /// which this process becomes, or by that monitor going.
    Outdated(String),
    /// A monitor of this build refused this client on its merits. Ended by
    /// a config edit or by that monitor going.
    Refused(String),
    /// None could be started, none admitted this client, or the one it had
    /// went before answering. Ended by a config edit, an install or a
    /// monitor publishing; a start that failed for a transient reason
    /// changes none of those, and only the report ends it.
    Unstartable(String),
}

impl Unserved {
    pub(crate) fn why(&self) -> &str {
        match self {
            Unserved::Retiring(why)
            | Unserved::Repairing(why)
            | Unserved::Outdated(why)
            | Unserved::Refused(why)
            | Unserved::Unstartable(why) => why,
        }
    }

    /// What can end it.
    pub(crate) fn sources(&self) -> &'static [Source] {
        match self {
            Unserved::Retiring(_) => &[Source::Lease, Source::Published],
            Unserved::Repairing(_) => &[Source::Permit, Source::Published],
            Unserved::Outdated(_) => &[Source::Install, Source::Published],
            Unserved::Refused(_) => &[Source::Config, Source::Published],
            Unserved::Unstartable(_) => &[Source::Config, Source::Install, Source::Published],
        }
    }
}

impl Attempt {
    /// The connection, or why a waiter of `repo` that never decides for
    /// itself has none this time.
    ///
    /// A retirement is read from what is known of the repo, not from how
    /// this client's own attempt happened to end. A lease still held says
    /// so; so does a monitor of another build still published for the
    /// repo, whichever client is repairing and however this one's turn
    /// behind it went. Only a refusal by a newer monitor is left as it
    /// is: there this process is the one that is out of date.
    pub(crate) fn served(self, repo: &Path) -> Result<Conn, Unserved> {
        let unless_going = |class: fn(String) -> Unserved, why| match outgoing(repo) {
            true => Unserved::Retiring(why),
            false => class(why),
        };
        Err(match self {
            Attempt::Shared(conn) => return Ok(conn),
            Attempt::LeaseHeld(why) => Unserved::Retiring(why),
            Attempt::Newer(why) => Unserved::Outdated(why),
            Attempt::Busy(why) => unless_going(Unserved::Repairing, why),
            Attempt::Theirs(why) => unless_going(Unserved::Refused, why),
            Attempt::Unproven(why)
            | Attempt::SpawnFailed(why)
            | Attempt::NotStarted(why)
            | Attempt::Unattached(why) => unless_going(Unserved::Unstartable, why),
        })
    }
}

/// A waiter's time without a monitor, and the one deadline by which it
/// is reported.
#[derive(Debug, Default)]
pub(crate) struct Outage {
    /// Owed from the first loss until an answer or the report: a later
    /// loss can bring it nearer, never put it off.
    report_by: Option<tokio::time::Instant>,
    /// A monitor answered on the connection now held.
    served: bool,
    reported: bool,
}

impl Outage {
    /// A monitor answered, so there is no outage.
    pub(crate) fn answered(&mut self) {
        *self = Outage {
            served: true,
            ..Outage::default()
        };
    }

    /// The connection ended: whether to attempt again at once, which only
    /// one a monitor answered on earns. One that ended unanswered is a
    /// failed attempt.
    pub(crate) fn left(&mut self) -> bool {
        std::mem::take(&mut self.served)
    }

    /// An attempt ended `unserved` at `now`: when the outage is to be
    /// reported if no monitor answers first, or `None` once it has been.
    ///
    /// The earliest of what each loss allows: [`RETIREMENT_BUDGET`] after a
    /// retirement, [`OUTAGE_BUDGET`] after anything else. Neither failed
    /// attempts, the events between them, nor a change of class postpone
    /// it.
    pub(crate) fn lost(
        &mut self,
        now: tokio::time::Instant,
        unserved: &Unserved,
    ) -> Option<tokio::time::Instant> {
        self.served = false;
        let allowed = now
            + match unserved {
                Unserved::Retiring(_) => RETIREMENT_BUDGET,
                _ => OUTAGE_BUDGET,
            };
        let by = self.report_by.map_or(allowed, |by| by.min(allowed));
        self.report_by = Some(by);
        (!self.reported).then_some(by)
    }

    /// When the outage is owed its report, if it is.
    pub(crate) fn report_by(&self) -> Option<tokio::time::Instant> {
        self.report_by.filter(|_| !self.reported)
    }

    /// The outage was reported, or its report was owed nobody: from now
    /// on only an event ends it.
    pub(crate) fn reported(&mut self) {
        self.reported = true;
    }
}

/// What can end a waiter's time without a monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Source {
    /// The repo's monitor lease, released by a monitor going.
    Lease,
    /// The repo's repair permit, released by the client that holds it.
    Permit,
    /// The repo's descriptor in the runtime directory: a monitor
    /// publishing, or going.
    Published,
    /// The installation.
    Install,
    /// The repo's and the user's config files.
    Config,
}

impl Source {
    /// A lock's release is heard by blocking on it, so one already free is
    /// heard at once: arming it needs no second look, and once heard it is
    /// armed again.
    fn lock(self) -> bool {
        matches!(self, Source::Lease | Source::Permit)
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Source::Lease => "the repo's monitor lease",
            Source::Permit => "the repo's repair permit",
            Source::Published => "the repo's published monitor",
            Source::Install => "the installation",
            Source::Config => "the config files",
        })
    }
}

/// What an armed source says.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Heard {
    Moved(Source),
    /// Its watch failed or stopped: nothing it would report reaches the
    /// waiter any more.
    Ended(Source, String),
}

type Sink = Arc<dyn Fn(Heard) + Send + Sync>;

/// How one armed source tells its waiter. Dropped before its watch said
/// it ended, it says so itself, so a watch that dies is never taken for
/// one that is quiet.
pub(crate) struct Tell {
    source: Source,
    sink: Sink,
    done: AtomicBool,
}

impl Tell {
    pub(crate) fn moved(&self) {
        if self.source.lock() && self.done.swap(true, Ordering::AcqRel) {
            return;
        }
        (self.sink)(Heard::Moved(self.source));
    }

    pub(crate) fn failed(&self, why: impl std::fmt::Display) {
        if !self.done.swap(true, Ordering::AcqRel) {
            (self.sink)(Heard::Ended(self.source, why.to_string()));
        }
    }
}

impl Drop for Tell {
    fn drop(&mut self) {
        self.failed("its watch stopped");
    }
}

/// Where a waiter's sources are heard from.
pub(crate) trait Ears: Send + Sync {
    /// Hear `source` through `tell` until the returned guard is dropped.
    fn arm(&self, source: Source, tell: Tell) -> Result<Box<dyn Send>, String>;
}

/// The sources one outage has armed, kept until a monitor answers: one
/// armed for an earlier failure may still end a later one.
pub(crate) struct Hearing {
    sink: Sink,
    live: Arc<AtomicBool>,
    armed: BTreeMap<Source, Box<dyn Send>>,
}

impl Hearing {
    pub(crate) fn new(sink: impl Fn(Heard) + Send + Sync + 'static) -> Self {
        let live = Arc::new(AtomicBool::new(true));
        let sink: Sink = {
            let live = live.clone();
            Arc::new(move |heard| {
                if live.load(Ordering::Acquire) {
                    sink(heard)
                }
            })
        };
        Hearing {
            sink,
            live,
            armed: BTreeMap::new(),
        }
    }

    /// Arm what `unserved` waits on that is not armed yet. `Ok(true)` when
    /// a watch was newly armed whose event may already have passed, so the
    /// caller attempts once more before it waits.
    pub(crate) fn arm(&mut self, ears: &impl Ears, unserved: &Unserved) -> Result<bool, String> {
        let mut look_again = false;
        for &source in unserved.sources() {
            if self.armed.contains_key(&source) {
                continue;
            }
            let tell = Tell {
                source,
                sink: self.sink.clone(),
                done: AtomicBool::new(false),
            };
            let guard = ears
                .arm(source, tell)
                .map_err(|e| format!("cannot watch {source}: {e}"))?;
            self.armed.insert(source, guard);
            look_again |= !source.lock();
        }
        Ok(look_again)
    }

    /// What `heard` means: a move to attempt on, or why nothing can be
    /// heard any more.
    pub(crate) fn heard(&mut self, heard: Heard) -> Result<(), String> {
        match heard {
            Heard::Moved(source) => {
                if source.lock() {
                    self.armed.remove(&source);
                }
                Ok(())
            }
            Heard::Ended(source, why) => Err(format!("the watch on {source} ended: {why}")),
        }
    }
}

impl Drop for Hearing {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
    }
}

/// What a waiter without a monitor does next.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Next {
    Attempt,
    /// The outage's deadline passed with nothing to end it.
    Report,
    /// A source can no longer be heard, and why.
    Deaf(String),
}

/// An async waiter's [`Outage`] and what it hears.
pub(crate) struct Absence<E> {
    ears: E,
    outage: Outage,
    listening: Option<(Hearing, tokio::sync::mpsc::UnboundedReceiver<Heard>)>,
}

impl<E: Ears> Absence<E> {
    pub(crate) fn new(ears: E) -> Self {
        Absence {
            ears,
            outage: Outage::default(),
            listening: None,
        }
    }

    /// A monitor answered: the outage and everything armed for it end.
    pub(crate) fn answered(&mut self) {
        self.outage.answered();
        self.listening = None;
    }

    /// See [`Outage::left`].
    pub(crate) fn left(&mut self) -> bool {
        self.outage.left()
    }

    /// See [`Outage::reported`].
    pub(crate) fn reported(&mut self) {
        self.outage.reported();
    }

    /// After an attempt that ended `unserved`: arm what can end it, and
    /// say when to attempt again, or that the outage is due its report,
    /// or that it can no longer be heard.
    pub(crate) async fn after(&mut self, unserved: &Unserved) -> Next {
        let now = tokio::time::Instant::now();
        let report_by = self.outage.lost(now, unserved);
        if report_by.is_some_and(|by| by <= now) {
            return Next::Report;
        }
        let (hearing, heard) = self.listening.get_or_insert_with(|| {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            (Hearing::new(move |heard| drop(tx.send(heard))), rx)
        });
        match hearing.arm(&self.ears, unserved) {
            Err(why) => return Next::Deaf(why),
            Ok(true) => return Next::Attempt,
            Ok(false) => {}
        }
        let report = async {
            match report_by {
                Some(by) => tokio::time::sleep_until(by).await,
                None => std::future::pending().await,
            }
        };
        // The deadline first: a move heard after it is left in the channel
        // for the next call, once the report is made.
        tokio::select! {
            biased;
            () = report => Next::Report,
            heard = heard.recv() => match heard.map(|heard| hearing.heard(heard)) {
                Some(Ok(())) => Next::Attempt,
                Some(Err(why)) => Next::Deaf(why),
                None => Next::Deaf("every watch stopped".into()),
            },
        }
    }
}

/// The kernel's notices for `repo`'s sources: a blocked flock for a lock,
/// a watch for files, and the installation as `authority` watches it.
pub(crate) struct Kernel {
    repo: PathBuf,
    authority: crate::upgrade::Authority,
}

impl Kernel {
    pub(crate) fn new(repo: &Path, authority: crate::upgrade::Authority) -> Self {
        Kernel {
            repo: repo.to_path_buf(),
            authority,
        }
    }
}

impl Ears for Kernel {
    fn arm(&self, source: Source, tell: Tell) -> Result<Box<dyn Send>, String> {
        match source {
            Source::Lease => Ok(released(super::runtime_path(&self.repo, "lock"), tell)),
            Source::Permit => Ok(released(super::runtime_path(&self.repo, "repair"), tell)),
            Source::Published => {
                super::ensure_runtime_dir()?;
                watch_files(vec![super::runtime_path(&self.repo, "json")], tell)
            }
            Source::Config => {
                let mut files = vec![crate::agent_store::repo_config::path(&self.repo)];
                if let Some(home) = std::env::var_os("HOME") {
                    files.push(clank_user_config::path(Path::new(&home)));
                }
                watch_files(files, tell)
            }
            Source::Install => {
                let tell = Arc::new(tell);
                let (moved, failed) = (tell.clone(), tell.clone());
                match self
                    .authority
                    .subscribe(move || moved.moved(), move |why| failed.failed(why))?
                {
                    Some(subscription) => Ok(Box::new(subscription)),
                    // Nothing watched is a process never replaced: a source
                    // that never moves, kept so it is not read as one that died.
                    None => Ok(Box::new(tell)),
                }
            }
        }
    }
}

/// A thread of its own, not the runtime's blocking pool, for the reason
/// `IngestLease::when_free` gives.
fn released(path: PathBuf, tell: Tell) -> Box<dyn Send> {
    std::thread::spawn(
        move || match crate::cli::github::log::when_released(&path, true) {
            Ok(()) => tell.moved(),
            Err(e) => tell.failed(e),
        },
    );
    Box::new(())
}

fn watch_files(files: Vec<PathBuf>, tell: Tell) -> Result<Box<dyn Send>, String> {
    use notify::Watcher as _;
    let content = files.clone();
    let named = files.clone();
    let mut watcher = crate::proc_identity::tree_watch::KernelWatcher::new(
        move |file| content.iter().any(|c| c == file),
        move |res: notify::Result<notify::Event>| {
            use crate::fs_notice::Notice;
            match crate::fs_notice::notice(&res) {
                Notice::Changed(paths) if paths.iter().any(|p| named.contains(p)) => tell.moved(),
                Notice::Changed(_) | Notice::Read => {}
                Notice::Rescan => tell.moved(),
                Notice::Failed(why) => tell.failed(why),
            }
        },
    )
    .map_err(|e| e.to_string())?;
    for file in &files {
        let dir = file
            .parent()
            .ok_or_else(|| format!("{} has no directory", file.display()))?;
        watcher
            .watch(dir, notify::RecursiveMode::NonRecursive)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    Ok(Box::new(watcher))
}

/// Ears a test drives: each source arms unless refused, and the test moves
/// or ends what is armed.
#[cfg(test)]
pub(crate) mod scripted {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Inner {
        armed: Vec<(u64, Source, Option<Tell>)>,
        next: u64,
        refuse: Option<String>,
        arming: Option<Box<dyn FnOnce() + Send>>,
        arms: Vec<Source>,
    }

    #[derive(Clone, Default)]
    pub(crate) struct Scripted(Arc<Mutex<Inner>>);

    struct Guard(Arc<Mutex<Inner>>, u64);

    impl Drop for Guard {
        fn drop(&mut self) {
            let tell = {
                let mut inner = self.0.lock().unwrap();
                let at = inner.armed.iter().position(|(id, ..)| *id == self.1);
                at.map(|at| inner.armed.remove(at))
            };
            drop(tell);
        }
    }

    impl Scripted {
        fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
            self.0.lock().unwrap()
        }

        /// Move every armed `source`; whether any was armed.
        pub(crate) fn fire(&self, source: Source) -> bool {
            let inner = self.inner();
            let mut fired = false;
            for (_, _, tell) in inner.armed.iter().filter(|(_, s, _)| *s == source) {
                if let Some(tell) = tell {
                    tell.moved();
                    fired = true;
                }
            }
            fired
        }

        /// Drop every armed `source`'s tell, as a watch that died does.
        pub(crate) fn end(&self, source: Source) {
            let tells: Vec<Tell> = {
                let mut inner = self.inner();
                inner
                    .armed
                    .iter_mut()
                    .filter(|(_, s, _)| *s == source)
                    .filter_map(|(_, _, tell)| tell.take())
                    .collect()
            };
            drop(tells);
        }

        /// Refuse every arming from now on.
        pub(crate) fn refuse(&self, why: &str) {
            self.inner().refuse = Some(why.into());
        }

        /// Run `f` as the next source is armed, before its watch exists.
        pub(crate) fn while_arming(&self, f: impl FnOnce() + Send + 'static) {
            self.inner().arming = Some(Box::new(f));
        }

        /// Every arming so far, in order.
        pub(crate) fn arms(&self) -> Vec<Source> {
            self.inner().arms.clone()
        }

        /// What is armed now.
        pub(crate) fn armed(&self) -> Vec<Source> {
            self.inner().armed.iter().map(|(_, s, _)| *s).collect()
        }
    }

    impl Ears for Scripted {
        fn arm(&self, source: Source, tell: Tell) -> Result<Box<dyn Send>, String> {
            let arming = self.inner().arming.take();
            if let Some(arming) = arming {
                arming();
            }
            let mut inner = self.inner();
            if let Some(why) = inner.refuse.clone() {
                return Err(why);
            }
            inner.next += 1;
            let id = inner.next;
            inner.armed.push((id, source, Some(tell)));
            inner.arms.push(source);
            Ok(Box::new(Guard(self.0.clone(), id)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::scripted::Scripted;
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    fn failed() -> Unserved {
        Unserved::Unstartable("why".into())
    }

    /// The deadline runs from the first failure until a monitor answers:
    /// failed attempts at any pace leave it where it was, and an answer
    /// ends the outage, so the next loss begins a new one.
    #[tokio::test(start_paused = true)]
    async fn an_outages_deadline_stays_put_until_an_answer_ends_it() {
        let mut outage = Outage::default();
        let began = tokio::time::Instant::now();
        let by = began + OUTAGE_BUDGET;
        for secs in [0, 1, 7, 30, 59] {
            assert_eq!(
                outage.lost(began + Duration::from_secs(secs), &failed()),
                Some(by)
            );
        }
        outage.answered();
        let later = began + OUTAGE_BUDGET * 2;
        assert_eq!(outage.lost(later, &failed()), Some(later + OUTAGE_BUDGET));
    }

    /// A monitor known to be on its way out is reported as late as it and
    /// its successor are allowed, which is later than an outage is, and a
    /// retirement seen again counts from the first loss.
    #[tokio::test(start_paused = true)]
    async fn a_retirement_is_given_what_it_is_allowed() {
        let retiring = Unserved::Retiring("its lease is still held".into());
        assert_eq!(
            RETIREMENT_BUDGET,
            super::super::QUIESCE_BUDGET
                + super::super::LEASE_RETIREMENT
                + super::super::SETTLE_BUDGET
                + super::super::SPAWN_SETTLE
                + super::super::ATTACH_BUDGET,
            "what the old monitor may take, and the new one to start"
        );
        assert!(RETIREMENT_BUDGET > OUTAGE_BUDGET);
        let mut outage = Outage::default();
        let began = tokio::time::Instant::now();
        let by = Some(began + RETIREMENT_BUDGET);
        assert_eq!(outage.lost(began, &retiring), by);
        let later = began + RETIREMENT_BUDGET - Duration::from_secs(1);
        assert_eq!(outage.lost(later, &retiring), by);
    }

    /// A change of class never postpones the report: not a retirement seen
    /// a second before a failure's deadline, nor a failure a second before
    /// a retirement's. One that allows less brings it nearer.
    #[tokio::test(start_paused = true)]
    async fn a_change_of_class_never_postpones_the_report() {
        let retiring = || Unserved::Retiring("its lease is still held".into());
        let second = Duration::from_secs(1);
        let began = tokio::time::Instant::now();

        let mut outage = Outage::default();
        let by = outage.lost(began, &failed()).unwrap();
        assert_eq!(outage.lost(by - second, &retiring()), Some(by));
        assert_eq!(outage.lost(by - second, &failed()), Some(by));
        assert_eq!(outage.report_by(), Some(by));

        let mut outage = Outage::default();
        let by = outage.lost(began, &retiring()).unwrap();
        assert_eq!(outage.lost(by - second, &failed()), Some(by));
        assert_eq!(outage.lost(by - second, &retiring()), Some(by));

        let mut outage = Outage::default();
        outage.lost(began, &retiring());
        assert_eq!(
            outage.lost(began + second, &failed()),
            Some(began + second + OUTAGE_BUDGET),
            "a failure that allows less brings the report nearer"
        );
    }

    /// Once reported, an outage has no deadline left; an answer begins the
    /// next one with its own.
    #[tokio::test(start_paused = true)]
    async fn a_reported_outage_has_no_deadline_until_an_answer() {
        let mut outage = Outage::default();
        let now = tokio::time::Instant::now();
        assert!(outage.lost(now, &failed()).is_some());
        outage.reported();
        assert_eq!(outage.lost(now + OUTAGE_BUDGET * 3, &failed()), None);
        outage.answered();
        assert!(outage.lost(now, &failed()).is_some());
    }

    /// Only a connection a monitor answered on is followed at once.
    #[test]
    fn only_an_answered_connection_is_followed_at_once() {
        let mut outage = Outage::default();
        assert!(!outage.left(), "a connection nobody answered on");
        outage.answered();
        assert!(outage.left());
        assert!(!outage.left(), "and only once per answer");
    }

    /// A monitor going is known from the repo, whichever way a client's
    /// own attempt ended: behind another client's repair, unattached,
    /// unable to start. With a monitor of another build still published
    /// and alive, each of those is a retirement. A descriptor of this
    /// build, or of a monitor that is dead, makes none of them one; and a
    /// newer monitor's refusal never is.
    #[test]
    fn a_retirement_is_read_from_the_repo_not_from_who_won_the_repair() {
        use super::super::{DescribedBuild, Descriptor, PROTO, TempRepo, describe, publish};
        let why = || "why".to_string();
        let contenders = || {
            [
                Attempt::Theirs(why()),
                Attempt::Unproven(why()),
                Attempt::Busy(why()),
                Attempt::SpawnFailed(why()),
                Attempt::NotStarted(why()),
                Attempt::Unattached(why()),
            ]
        };
        let me = std::process::id() as i32;
        let other_build = |repo: &Path, pid: i32| Descriptor {
            build: Some(DescribedBuild {
                running: "00000000000000000000000000000000".into(),
                version: None,
                installed_at: "/bin/clank".into(),
                source: crate::build_stamp::Source::Invocation,
            }),
            ..describe(repo, pid, Path::new("/nowhere.sock"))
        };

        let going = TempRepo::new();
        publish(going.path(), &other_build(going.path(), me)).unwrap();
        assert!(outgoing(going.path()));
        for attempt in contenders() {
            assert_eq!(
                attempt.served(going.path()).err(),
                Some(Unserved::Retiring(why()))
            );
        }
        assert_eq!(
            Attempt::Newer(why()).served(going.path()).err(),
            Some(Unserved::Outdated(why())),
            "there this process is the one out of date"
        );

        let older = TempRepo::new();
        publish(
            older.path(),
            &Descriptor {
                proto: PROTO - 1,
                ..describe(older.path(), me, Path::new("/nowhere.sock"))
            },
        )
        .unwrap();
        assert!(outgoing(older.path()), "another protocol is another build");

        let dead = TempRepo::new();
        publish(dead.path(), &other_build(dead.path(), i32::MAX - 7)).unwrap();
        let ours = TempRepo::new();
        publish(
            ours.path(),
            &Descriptor {
                build: Some(DescribedBuild {
                    running: crate::build_stamp::running().to_string(),
                    version: None,
                    installed_at: "/bin/clank".into(),
                    source: crate::build_stamp::Source::Invocation,
                }),
                ..describe(ours.path(), me, Path::new("/nowhere.sock"))
            },
        )
        .unwrap();
        for settled in [&dead, &ours] {
            assert!(!outgoing(settled.path()));
            for attempt in contenders() {
                assert!(
                    !matches!(
                        attempt.served(settled.path()).err(),
                        Some(Unserved::Retiring(_))
                    ),
                    "nothing says a monitor is going"
                );
            }
        }
    }

    /// Each way an attempt ends without a connection is classed by what
    /// can end it.
    #[test]
    fn each_unattached_attempt_is_classed_by_what_can_end_it() {
        let repo = super::super::TempRepo::new();
        let why = || "why".to_string();
        for (attempt, class) in [
            (Attempt::LeaseHeld(why()), Unserved::Retiring(why())),
            (Attempt::Busy(why()), Unserved::Repairing(why())),
            (Attempt::Newer(why()), Unserved::Outdated(why())),
            (Attempt::Theirs(why()), Unserved::Refused(why())),
            (Attempt::Unproven(why()), Unserved::Unstartable(why())),
            (Attempt::SpawnFailed(why()), Unserved::Unstartable(why())),
            (Attempt::NotStarted(why()), Unserved::Unstartable(why())),
            (Attempt::Unattached(why()), Unserved::Unstartable(why())),
        ] {
            assert_eq!(attempt.served(repo.path()).err(), Some(class));
        }
    }

    /// A waiter with `ears` whose attempts end as `attempt` says, counted:
    /// what [`Absence::after`] said each time, until it says other than
    /// to attempt again or `limit` attempts were made.
    async fn after_each(
        ears: &Scripted,
        attempts: &AtomicUsize,
        mut attempt: impl FnMut(usize) -> Option<Unserved>,
    ) -> Next {
        let mut absence = Absence::new(ears.clone());
        loop {
            let nth = attempts.fetch_add(1, Ordering::SeqCst);
            let Some(unserved) = attempt(nth) else {
                return Next::Attempt;
            };
            match absence.after(&unserved).await {
                Next::Attempt => continue,
                other => return other,
            }
        }
    }

    /// A monitor that appears after a failed attempt and before the watch
    /// for it is armed is found: arming is followed by one more attempt.
    #[tokio::test(start_paused = true)]
    async fn a_monitor_published_before_its_watch_is_armed_is_found() {
        let ears = Scripted::default();
        let appeared = Arc::new(AtomicBool::new(false));
        ears.while_arming({
            let appeared = appeared.clone();
            move || appeared.store(true, Ordering::SeqCst)
        });
        let attempts = AtomicUsize::new(0);
        let began = tokio::time::Instant::now();
        let ended = after_each(&ears, &attempts, |_| {
            (!appeared.load(Ordering::SeqCst)).then(failed)
        })
        .await;
        assert_eq!(ended, Next::Attempt, "it attached");
        assert_eq!(attempts.into_inner(), 2);
        assert_eq!(began.elapsed(), Duration::ZERO, "with no wait at all");
    }

    /// A held lease's release is what the waiter attempts on, and its
    /// next attempt attaches.
    #[tokio::test(start_paused = true)]
    async fn a_held_leases_release_leads_to_an_attach() {
        let ears = Scripted::default();
        let released = Arc::new(AtomicBool::new(false));
        let attempts = AtomicUsize::new(0);
        let retiring = || Unserved::Retiring("its lease is still held".into());
        let waiting = after_each(&ears, &attempts, |_| {
            (!released.load(Ordering::SeqCst)).then(retiring)
        });
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_secs(30), &mut waiting)
                .await
                .is_err()
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "the first, and the look after the descriptor's watch was armed"
        );
        released.store(true, Ordering::SeqCst);
        assert!(ears.fire(Source::Lease));
        assert_eq!(waiting.await, Next::Attempt);
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            3,
            "one attempt for the release"
        );
    }

    /// A lease held forever, with nothing else moving: the outage is
    /// reported once, at its deadline, and nothing is attempted between.
    #[tokio::test(start_paused = true)]
    async fn a_lease_held_forever_is_reported_at_its_deadline_with_no_attempt_between() {
        let ears = Scripted::default();
        let attempts = AtomicUsize::new(0);
        let began = tokio::time::Instant::now();
        let ended = after_each(&ears, &attempts, |_| {
            Some(Unserved::Retiring("its lease is still held".into()))
        })
        .await;
        assert_eq!(ended, Next::Report);
        assert_eq!(began.elapsed(), RETIREMENT_BUDGET);
        assert_eq!(
            attempts.into_inner(),
            2,
            "the first, and the look on arming"
        );
    }

    /// Once reported, a waiter attempts nothing and wakes for nothing while
    /// nothing changes, and attempts once a source of its class moves.
    #[tokio::test(start_paused = true)]
    async fn a_reported_outage_waits_on_its_events_alone() {
        let ears = Scripted::default();
        let mut absence = Absence::new(ears.clone());
        assert_eq!(absence.after(&failed()).await, Next::Attempt);
        assert_eq!(absence.after(&failed()).await, Next::Report);
        absence.reported();
        let began = tokio::time::Instant::now();
        let parked = tokio::time::timeout(OUTAGE_BUDGET * 10, absence.after(&failed())).await;
        assert!(parked.is_err(), "it ended: {parked:?}");
        assert_eq!(
            ears.arms(),
            [Source::Config, Source::Install, Source::Published],
            "each armed once, and nothing armed again"
        );
        assert_eq!(began.elapsed(), OUTAGE_BUDGET * 10);
        assert!(ears.fire(Source::Config));
        assert_eq!(absence.after(&failed()).await, Next::Attempt);
    }

    /// A watch that cannot be armed, and one that stops, each end the
    /// wait with why, and neither is taken for a quiet one.
    #[tokio::test(start_paused = true)]
    async fn a_watch_refused_or_stopped_ends_the_wait() {
        let ears = Scripted::default();
        ears.refuse("no descriptors left");
        let mut absence = Absence::new(ears.clone());
        let began = tokio::time::Instant::now();
        assert_eq!(
            absence.after(&failed()).await,
            Next::Deaf("cannot watch the config files: no descriptors left".into())
        );
        assert_eq!(began.elapsed(), Duration::ZERO);

        let ears = Scripted::default();
        let mut absence = Absence::new(ears.clone());
        assert_eq!(absence.after(&failed()).await, Next::Attempt);
        let failure = failed();
        let waiting = absence.after(&failure);
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), &mut waiting)
                .await
                .is_err()
        );
        ears.end(Source::Published);
        assert_eq!(
            waiting.await,
            Next::Deaf("the watch on the repo's published monitor ended: its watch stopped".into())
        );
        assert_eq!(began.elapsed(), Duration::from_secs(1));
    }

    /// A move heard after the deadline is reported first, and attempted
    /// on after: it is neither an attempt that skips the report nor lost.
    #[tokio::test(start_paused = true)]
    async fn a_move_heard_after_the_deadline_is_reported_before_it_is_attempted() {
        let ears = Scripted::default();
        let mut absence = Absence::new(ears.clone());
        assert_eq!(absence.after(&failed()).await, Next::Attempt);
        tokio::time::advance(OUTAGE_BUDGET).await;
        assert!(ears.fire(Source::Config));
        assert_eq!(absence.after(&failed()).await, Next::Report);
        absence.reported();
        assert_eq!(absence.after(&failed()).await, Next::Attempt);

        let mut absence = Absence::new(ears.clone());
        assert_eq!(absence.after(&failed()).await, Next::Attempt);
        let by = tokio::time::Instant::now() + OUTAGE_BUDGET;
        let moves_at_the_deadline = async {
            tokio::time::sleep_until(by).await;
            assert!(ears.fire(Source::Config));
        };
        let failure = failed();
        let ((), next) = tokio::join!(biased; moves_at_the_deadline, absence.after(&failure));
        assert_eq!(
            next,
            Next::Report,
            "a move at the very deadline waits for the report"
        );
        absence.reported();
        assert_eq!(absence.after(&failed()).await, Next::Attempt);
    }

    /// The production ears on a process that watches no installation: the
    /// installation is a source that never moves, and never one that died.
    #[test]
    fn the_kernel_hears_an_unwatched_installation_as_quiet() {
        let repo = super::super::TempRepo::new();
        let kernel = Kernel::new(repo.path(), crate::upgrade::Authority::unwatched());
        let (tx, rx) = std::sync::mpsc::channel();
        let mut hearing = Hearing::new(move |heard| drop(tx.send(heard)));
        assert_eq!(
            hearing.arm(&kernel, &Unserved::Outdated("newer".into())),
            Ok(true)
        );
        assert_eq!(rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
    }

    /// The kernel's ears hear a held lease's release, with the lock held
    /// through the test's registry rather than by a descriptor a child
    /// could inherit. On the real clock: a paused one would leap to the
    /// report deadline while the release's thread is still on its way.
    #[tokio::test]
    async fn the_kernel_hears_a_held_lease_released() {
        let repo = super::super::TempRepo::new();
        super::super::ensure_runtime_dir().unwrap();
        let lease = super::super::runtime_path(repo.path(), "lock");
        let held = crate::cli::github::log::tests::hold_paths(std::slice::from_ref(&lease));
        let mut absence = Absence::new(Kernel::new(
            repo.path(),
            crate::upgrade::Authority::unwatched(),
        ));
        let retiring = Unserved::Retiring("its lease is still held".into());
        assert_eq!(absence.after(&retiring).await, Next::Attempt);
        let waiting = absence.after(&retiring);
        tokio::pin!(waiting);
        while crate::cli::github::log::tests::blocked(&lease) == 0 {
            assert!(
                tokio::time::timeout(Duration::from_millis(10), &mut waiting)
                    .await
                    .is_err(),
                "nothing moved while the lease is held"
            );
        }
        held.release();
        assert_eq!(waiting.await, Next::Attempt);
    }
}
