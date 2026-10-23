//! The monitor's side of user commands: the store watch that wakes it,
//! the attempts it owns, and the real seams an attempt runs on.

use super::dispatch::{self, Ended, Job, Seams};
use super::envelope::{Id, SiteKey};
use super::store::{PaneSeen, Stamp, Store};
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
    _watcher: Option<notify::RecommendedWatcher>,
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
            Ok(dir) => watch(&dir, changed_tx)
                .map_err(|e| eprintln!("monitor: cannot watch user commands: {e:#}"))
                .ok(),
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
        let key = super::site_key(&self.home);
        let repo = self.repo.clone();
        let history =
            |label: &AgentLabel, record: nudge_log::Record| record_history(&repo, label, &record);
        let ctx = dispatch::Ctx {
            store: &self.store,
            tag: &self.tag,
            repo: &self.repo,
            key: key.as_ref(),
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
) -> notify::Result<notify::RecommendedWatcher> {
    use notify::Watcher;
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && changes_a_record(&event)
        {
            let _ = changed.send(());
        }
    })?;
    watcher.watch(dir, notify::RecursiveMode::Recursive)?;
    Ok(watcher)
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

impl Seams for Live {
    fn now(&self) -> u64 {
        crate::cli::recovery::unix_now()
    }

    fn key(&self) -> Option<SiteKey> {
        super::site_key(&self.home)
    }

    fn binding(&self, label: &AgentLabel) -> Option<(Tool, SessionId)> {
        crate::agent_store::load_agent_config(&self.repo, label)
            .ok()
            .flatten()
            .and_then(|c| c.session)
            .map(|s| (s.tool, s.id))
    }

    async fn discover(&self, label: &AgentLabel) -> Result<(PaneSeen, Tool), String> {
        let (found, tool, process) =
            crate::cli::pane_target::discover_process(self.repo.clone(), label.clone())
                .await
                .map_err(|e| format!("{e:#}"))?;
        Ok((
            PaneSeen {
                session: found.session,
                pane: found.pane,
                pid: process.pid,
                birth: process.birth,
            },
            tool,
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
        crate::cli::event_interrupt::blocking(|| {
            crate::cli::pane_input::lock_pane(&pane.session, &pane.pane)
        })
        .map(|lock| Box::new(lock) as Box<dyn Send>)
        .map_err(|e| format!("{e:#}"))
    }

    fn launch(
        &self,
        label: &AgentLabel,
        pane: &PaneSeen,
    ) -> Result<(Tool, crate::cli::pane_target::Process), String> {
        crate::cli::pane_target::verify_launch(&self.repo, label, &pane.session, &pane.pane)
            .map_err(|e| format!("{e:#}"))
    }

    type Term<'a> = crate::cli::pane_input::Zellij<'a>;

    fn terminal<'a>(&'a self, pane: &'a PaneSeen) -> Self::Term<'a> {
        crate::cli::pane_input::Zellij {
            session: &pane.session,
            pane: &pane.pane,
        }
    }

    fn evidence(&self) -> &[UserCommandEvidence] {
        crate::cli::pane_input::USER_COMMAND_EVIDENCE
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
