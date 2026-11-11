//! The monitor's side of user commands: the store watch that wakes it,
//! the attempts it owns, and the real seams an attempt runs on.

use super::dispatch::{self, Ended, Job, Seams};
use super::envelope::Id;
use super::store::{PaneSeen, Stamp, Store};
use super::trust::TrustSet;
use crate::cli::nudge_log;
use crate::cli::pane_input::UserCommandEvidence;
use clank_core::Tool;
use clank_core::ids::{AgentLabel, SessionId};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// How long a released park is given to end before its agent is looked at
/// as idle: the hook's exit and Claude's own Stop handling are not events
/// the monitor sees.
pub(crate) const RELEASE_SETTLE_SECS: u64 = 2;

/// An attempt's question to the loop: may it go on to the final checks?
pub(crate) struct Ask {
    pub(crate) label: AgentLabel,
    pub(crate) session: SessionId,
    pub(crate) reply: tokio::sync::oneshot::Sender<Result<(), String>>,
}

/// An attempt that ended.
pub(crate) struct Done {
    pub(crate) job: Job,
    pub(crate) ended: Ended,
}

/// The wakes user commands give the monitor's loop, each its own receiver
/// so the loop can select on all of them.
pub(crate) struct Wakes {
    pub(crate) changed: tokio::sync::mpsc::UnboundedReceiver<()>,
    pub(crate) asks: tokio::sync::mpsc::UnboundedReceiver<Ask>,
    pub(crate) done: tokio::sync::mpsc::UnboundedReceiver<Done>,
}

/// One repo's user commands, as its monitor holds them.
pub(crate) struct UserCommands {
    store: Store,
    tag: String,
    repo: PathBuf,
    home: PathBuf,
    asks: tokio::sync::mpsc::UnboundedSender<Ask>,
    done: tokio::sync::mpsc::UnboundedSender<Done>,
    in_flight: BTreeSet<(AgentLabel, Id)>,
    /// Requests waiting on a pane's input, and their record's identity
    /// then: looked at again only after a state change or a rewrite.
    on_screen: BTreeMap<(AgentLabel, Id), (Option<Stamp>, u64)>,
    /// When each agent's park was last released.
    released: BTreeMap<AgentLabel, u64>,
    next_due: Option<u64>,
    _watcher: Option<crate::proc_identity::tree_watch::KernelWatcher>,
}

impl UserCommands {
    /// The user commands of `repo`, watched in `store`. A watch that cannot
    /// be set up is reported; the loop's other wakes still drive passes.
    pub(crate) fn new(repo: &Path, home: PathBuf, store: Store) -> (Self, Wakes) {
        let tag = crate::cli::monitor::repo_tag(repo);
        let (changed_tx, changed) = tokio::sync::mpsc::unbounded_channel();
        let (asks_tx, asks) = tokio::sync::mpsc::unbounded_channel();
        let (done_tx, done) = tokio::sync::mpsc::unbounded_channel();
        let watcher = match store.ensure_repo(&tag) {
            Ok(dir) => match watch(&dir, changed_tx.clone()) {
                Ok(attached) => {
                    // A pass, for what changed before the watch.
                    let _ = changed_tx.send(());
                    Some(attached)
                }
                Err(e) => {
                    eprintln!("monitor: cannot watch user commands: {e:#}");
                    None
                }
            },
            Err(e) => {
                eprintln!("monitor: user commands are unavailable: {e:#}");
                None
            }
        };
        (
            UserCommands {
                store,
                tag,
                repo: repo.to_path_buf(),
                home,
                asks: asks_tx,
                done: done_tx,
                in_flight: BTreeSet::new(),
                on_screen: BTreeMap::new(),
                released: BTreeMap::new(),
                next_due: None,
                _watcher: watcher,
            },
            Wakes {
                changed,
                asks,
                done,
            },
        )
    }

    /// The next moment a pass is due with no other wake: a waiting
    /// request's expiry, or a released park's settling.
    pub(crate) fn next_due(&self) -> Option<u64> {
        self.next_due
    }

    /// A state change the loop saw: requests waiting on a pane's input may
    /// be looked at again.
    pub(crate) fn state_changed(&mut self) {
        self.on_screen.clear();
    }

    /// An attempt ended.
    pub(crate) fn finished(&mut self, done: Done) {
        self.in_flight
            .remove(&(done.job.label.clone(), done.job.id.clone()));
        if let Ended::OnScreen { stamp, expires } = done.ended {
            self.on_screen
                .insert((done.job.label, done.job.id), (stamp, expires));
            self.next_due = Some(self.next_due.map_or(expires, |due| due.min(expires)));
        }
    }

    /// The agents whose released park is still settling at `now`.
    pub(crate) fn settling(&self, now: u64) -> BTreeSet<AgentLabel> {
        self.released
            .iter()
            .filter(|(_, at)| now < *at + RELEASE_SETTLE_SECS)
            .map(|(label, _)| label.clone())
            .collect()
    }

    /// One pass over the repo's requests with the loop's view of its agents,
    /// spawning the attempts it hands out into `spawn`. What it decided.
    pub(crate) fn pass(
        &mut self,
        agents: &mut impl dispatch::Agents,
        spawn: &mut Vec<tokio::task::JoinHandle<()>>,
    ) -> dispatch::Pass {
        let now = crate::cli::recovery::unix_now();
        let trust = super::trust_set(&self.home);
        let repo = self.repo.clone();
        let history =
            |label: &AgentLabel, record: nudge_log::Record| record_history(&repo, label, &record);
        let ctx = dispatch::Ctx {
            store: &self.store,
            tag: &self.tag,
            repo: &self.repo,
            trust: &trust,
            now,
            history: &history,
        };
        let (store, tag) = (&self.store, &self.tag);
        let on_screen = &mut self.on_screen;
        on_screen.retain(|(label, id), (stamp, expires)| {
            now < *expires && store.stamp(tag, label, id) == *stamp
        });
        let (in_flight, waiting) = (&self.in_flight, &*on_screen);
        let out = dispatch::pass(&ctx, agents, |label, id| {
            let key = (label.clone(), id.clone());
            in_flight.contains(&key) || waiting.contains_key(&key)
        });
        for label in &out.release {
            self.released.insert(label.clone(), now);
        }
        self.released
            .retain(|label, at| out.release.contains(label) || now < *at + RELEASE_SETTLE_SECS);
        let settle = self
            .released
            .values()
            .map(|at| at + RELEASE_SETTLE_SECS)
            .filter(|at| *at > now)
            .min();
        let waiting = self.on_screen.values().map(|(_, expires)| *expires).min();
        self.next_due = [out.next_expiry, settle, waiting]
            .into_iter()
            .flatten()
            .min();
        for job in &out.jobs {
            if !self.in_flight.insert((job.label.clone(), job.id.clone())) {
                continue;
            }
            let live = Live {
                repo: self.repo.clone(),
                home: self.home.clone(),
                asks: self.asks.clone(),
            };
            let (store, tag, job, done) = (
                self.store.clone(),
                self.tag.clone(),
                job.clone(),
                self.done.clone(),
            );
            spawn.push(tokio::spawn(async move {
                let ended = dispatch::attempt(&store, &tag, &live.repo, &job, &live).await;
                let _ = done.send(Done { job, ended });
            }));
        }
        out
    }
}

fn record_history(repo: &Path, label: &AgentLabel, record: &nudge_log::Record) {
    let dir = crate::agent_store::agents_root(repo).join(label.as_str());
    if let Err(e) = nudge_log::append(&dir, record) {
        eprintln!("monitor: `{label}`'s nudge history: {e}");
    }
}

/// Watch `dir` for record files changing. Lock files and the store's own
/// temporaries are not changes.
fn watch(
    dir: &Path,
    changed: tokio::sync::mpsc::UnboundedSender<()>,
) -> notify::Result<crate::proc_identity::tree_watch::KernelWatcher> {
    use notify::Watcher;
    let mut watcher =
        crate::proc_identity::tree_watch::KernelWatcher::new(is_record, handler(changed))?;
    watcher.watch(dir, notify::RecursiveMode::Recursive)?;
    Ok(watcher)
}

/// [`watch`]'s callback, apart from any backend so a test can drive it. A
/// failed watch is said once and then stops waking anything, as one that
/// could not be set up does: the loop's other wakes still drive passes.
fn handler(
    changed: tokio::sync::mpsc::UnboundedSender<()>,
) -> impl FnMut(notify::Result<notify::Event>) + Send + 'static {
    use crate::fs_notice::{Notice, notice};
    let mut changed = Some(changed);
    move |res: notify::Result<notify::Event>| {
        let Some(wake) = &changed else {
            return;
        };
        match notice(&res) {
            Notice::Read => {}
            Notice::Rescan => {
                let _ = wake.send(());
            }
            Notice::Changed(_) => {
                if res.as_ref().is_ok_and(changes_a_record) {
                    let _ = wake.send(());
                }
            }
            Notice::Failed(why) => {
                eprintln!(
                    "monitor: the watch on user commands failed: {why}; their changes are \
                     seen only on the loop's other wakes from now on"
                );
                changed = None;
            }
        }
    }
}

/// Whether `event` is a record being written, replaced or removed. Reads
/// are not: inotify reports every open of a record a pass or `status`
/// makes, and waking on those would have each pass schedule the next.
pub(crate) fn changes_a_record(event: &notify::Event) -> bool {
    use notify::EventKind;
    use notify::event::ModifyKind;
    let mutation = matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_) | ModifyKind::Any)
    );
    mutation && event.paths.iter().any(|p| is_record(p))
}

/// Whether `path` is a record (`<id>.json`), not a lock or a temporary.
pub(crate) fn is_record(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "json")
        && path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| Id::parse(s).is_ok())
}

/// The real world an attempt runs in.
struct Live {
    repo: PathBuf,
    home: PathBuf,
    asks: tokio::sync::mpsc::UnboundedSender<Ask>,
}

/// Why a request recorded by an older clank is not written to.
const UNADDRESSED: &str = "its pane was recorded without its zellij server; it needs a new request";

/// A recorded pane's terminal: the pane in the server it was found in, or
/// none for an entry that never recorded one, which reads and writes
/// nothing.
pub(crate) struct Recorded<'a>(Option<crate::cli::pane_input::Zellij<'a>>);

impl crate::cli::pane_input::Terminal for Recorded<'_> {
    async fn dump(&self) -> Option<String> {
        self.0.as_ref()?.dump().await
    }

    async fn write(&self, argv: Vec<String>) -> Result<(), crate::cli::open_zellij::PaneWrite> {
        match &self.0 {
            Some(term) => term.write(argv).await,
            None => Err(crate::cli::open_zellij::PaneWrite::NotStarted(
                UNADDRESSED.to_string(),
            )),
        }
    }
}

impl Seams for Live {
    fn now(&self) -> u64 {
        crate::cli::recovery::unix_now()
    }

    fn trust(&self) -> TrustSet {
        super::trust_set(&self.home)
    }

    fn binding(&self, label: &AgentLabel) -> Option<(Tool, SessionId)> {
        crate::agent_store::load_agent_config(&self.repo, label)
            .ok()
            .flatten()
            .and_then(|c| c.session)
            .map(|s| (s.tool, s.id))
    }

    async fn discover(&self, label: &AgentLabel) -> Result<(PaneSeen, Tool), String> {
        let target = crate::cli::pane_target::discover(self.repo.clone(), label.clone())
            .await
            .map_err(|e| format!("{e:#}"))?;
        Ok((
            PaneSeen {
                session: target.session.to_string(),
                pane: target.pane,
                server: Some(target.session.endpoint().clone()),
            },
            target.tool,
        ))
    }

    async fn last_look(&self, label: &AgentLabel, session: &SessionId) -> Result<(), String> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.asks
            .send(Ask {
                label: label.clone(),
                session: session.clone(),
                reply,
            })
            .map_err(|_| "the monitor is gone".to_string())?;
        answer
            .await
            .unwrap_or_else(|_| Err("the monitor did not answer".into()))
    }

    async fn hold_binding<'a>(
        &'a self,
        label: &'a AgentLabel,
    ) -> Result<Box<dyn Send + 'a>, String> {
        crate::cli::event_interrupt::blocking(|| {
            crate::agent_store::hold_binding(&self.repo, label)
        })
        .map(|guard| Box::new(guard) as Box<dyn Send + 'a>)
        .map_err(|e| format!("{e:#}"))
    }

    async fn lock_pane(&self, pane: &PaneSeen) -> Result<Box<dyn Send>, String> {
        let server = pane
            .server
            .as_ref()
            .ok_or_else(|| UNADDRESSED.to_string())?;
        crate::cli::event_interrupt::blocking(|| {
            crate::cli::pane_input::lock_pane(server, &pane.pane)
        })
        .map(|lock| Box::new(lock) as Box<dyn Send>)
        .map_err(|e| format!("{e:#}"))
    }

    type Term<'a> = Recorded<'a>;

    fn terminal<'a>(&'a self, pane: &'a PaneSeen) -> Self::Term<'a> {
        Recorded(
            pane.server
                .as_ref()
                .map(|server| crate::cli::pane_input::Zellij {
                    session: server,
                    pane: &pane.pane,
                }),
        )
    }

    fn evidence(&self) -> Option<&UserCommandEvidence> {
        crate::cli::pane_input::USER_COMMAND_EVIDENCE.as_ref()
    }

    fn history(&self, label: &AgentLabel, record: nudge_log::Record) {
        record_history(&self.repo, label, &record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: notify::EventKind, paths: &[&str]) -> notify::Event {
        paths
            .iter()
            .fold(notify::Event::new(kind), |e, p| e.add_path(p.into()))
    }

    /// A rescan wakes the loop, since what it stands for is unknown, and a
    /// failed watch closes its channel rather than going on as if healthy.
    #[test]
    fn a_rescan_wakes_and_a_failure_stops_the_watch() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut handle = handler(tx);
        handle(Ok(notify::Event::new(notify::EventKind::Any)));
        assert_eq!(rx.try_recv(), Ok(()));
        handle(crate::fs_notice::tests::opened(std::path::Path::new(
            "/s/tag/claude/r1.json",
        )));
        assert!(rx.try_recv().is_err(), "a read");
        handle(Err(notify::Error::generic("the stream died")));
        assert_eq!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        );
    }

    /// Reading a record, as every pass and `status` do, wakes nothing,
    /// however the backend reports it; only writing, replacing or removing
    /// one does.
    #[test]
    fn reads_wake_nothing_and_writes_wake_the_loop() {
        use notify::EventKind::*;
        use notify::event::{
            AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind,
            RenameMode,
        };
        let record = "/s/tag/claude/r1.json";
        for read in [
            Access(AccessKind::Open(AccessMode::Read)),
            Access(AccessKind::Close(AccessMode::Read)),
            Access(AccessKind::Read),
            Access(AccessKind::Any),
            Modify(ModifyKind::Metadata(MetadataKind::AccessTime)),
            Other,
        ] {
            assert!(!changes_a_record(&event(read, &[record])), "{read:?}");
        }
        for write in [
            Create(CreateKind::File),
            Modify(ModifyKind::Data(DataChange::Content)),
            Modify(ModifyKind::Name(RenameMode::To)),
            Modify(ModifyKind::Name(RenameMode::Both)),
            Modify(ModifyKind::Name(RenameMode::Any)),
            Remove(RemoveKind::File),
        ] {
            assert!(changes_a_record(&event(write, &[record])), "{write:?}");
        }
    }

    /// The events an atomic save produces wake the loop once it lands on
    /// the record, and never for the temporary or the lock alone.
    #[test]
    fn an_atomic_replace_wakes_on_the_record_alone() {
        use notify::EventKind::*;
        use notify::event::{
            AccessKind, AccessMode, CreateKind, DataChange, ModifyKind, RenameMode,
        };
        let (tmp, lock, record) = (
            "/s/tag/claude/.uc-x1.tmp",
            "/s/tag/claude/r1.lock",
            "/s/tag/claude/r1.json",
        );
        let save = [
            event(Access(AccessKind::Open(AccessMode::Write)), &[lock]),
            event(Create(CreateKind::File), &[tmp]),
            event(Modify(ModifyKind::Data(DataChange::Any)), &[tmp]),
            event(Access(AccessKind::Close(AccessMode::Write)), &[tmp]),
            event(Modify(ModifyKind::Name(RenameMode::From)), &[tmp]),
            event(Modify(ModifyKind::Name(RenameMode::To)), &[record]),
        ];
        let woke: Vec<bool> = save.iter().map(changes_a_record).collect();
        assert_eq!(woke, [false, false, false, false, false, true]);
        let pass_reads = [
            event(Access(AccessKind::Open(AccessMode::Any)), &[lock]),
            event(Access(AccessKind::Open(AccessMode::Read)), &[record]),
            event(Access(AccessKind::Close(AccessMode::Read)), &[record]),
        ];
        assert!(
            !pass_reads.iter().any(changes_a_record),
            "a read-only pass schedules nothing"
        );
    }

    /// Through the kernel watch: a record saved in an agent's directory
    /// made after the watch began wakes the loop, and so does one written
    /// in place; reading one, or touching its lock, does not.
    #[test]
    fn the_watch_wakes_on_records_and_not_on_reads_or_locks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _watch = watch(&root, tx).unwrap();
        let woken = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<()>, ms| {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            let mut any = false;
            while rx.try_recv().is_ok() {
                any = true;
            }
            any
        };
        let agent = root.join("claude");
        std::fs::create_dir(&agent).unwrap();
        let _ = woken(&mut rx, 300);
        let tmp = agent.join(".uc-x1.tmp");
        let record = agent.join("r1.json");
        std::fs::write(&tmp, "{}").unwrap();
        std::fs::rename(&tmp, &record).unwrap();
        assert!(woken(&mut rx, 1000), "a record saved in a new agent dir");
        let _ = std::fs::read(&record).unwrap();
        std::fs::write(agent.join("r1.lock"), "").unwrap();
        assert!(!woken(&mut rx, 500), "a read or a lock");
        std::fs::write(&record, "{\"x\":1}").unwrap();
        assert!(woken(&mut rx, 1000), "a record written in place");
    }

    /// Only record files wake the loop: locks and the store's temporaries,
    /// which every save and every lock touch, do not.
    #[test]
    fn only_record_files_are_changes() {
        assert!(is_record(Path::new("/s/tag/claude/r1.json")));
        assert!(!is_record(Path::new("/s/tag/claude/r1.lock")));
        assert!(!is_record(Path::new("/s/tag/claude/.uc-abc.tmp")));
        assert!(!is_record(Path::new("/s/tag/claude")));
        assert!(!is_record(Path::new("/s/tag/claude/..json")));
    }
}
