//! The repo side of the browser bridge: pane subscriptions, transcript
//! tails, generated pages, and typed actions stay beside the repo.
//! The TUI uses [`BridgeInstance`] to publish this state to a separate
//! host process over the session bridge.
//!
//! `zellij web` shares a session at ONE geometry, sized to its
//! smallest client, so a phone gets the desktop's tab or shrinks the
//! desktop. This subscribes to each agent pane on its own instead:
//! the viewport arrives independent of the layout, and the page shows
//! one agent at a time. Terminals do not reflow, so a pane arrives at
//! its desktop size; the page scrolls and pinches around it.
//!
//! The default host owns browser admission and server controls.

mod bridge_instance;
pub(crate) mod claude_questions;
pub(crate) mod dns;
pub(crate) mod door;
mod feed;
mod freshness;
mod native_questions;
mod repo_session;
pub(crate) mod transcript;
mod transcript_images;
pub(crate) mod tunnel;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::cli::codex_chat;
use crate::cli::open_zellij::{self, PaneMeta, SubscribeChild};
use crate::cli::zellij_endpoint::Session;
pub(crate) use bridge_instance::AdminState;
pub(crate) use bridge_instance::BridgeInstance;
use feed::{Feed, SubscribeEvent, TableChange};

/// What the repo connector needs of zellij, behind a trait so an instance
/// runs in a test without one: the session, its panes, a
/// subscription to some of them, and a way to type into one.
pub(crate) trait Panes: Send + Sync + 'static {
    fn session(&self) -> &str;
    /// This repo's agent panes as the session shows them now; `None`
    /// when zellij does not answer.
    fn table(&self) -> Option<Vec<PaneMeta>>;
    fn subscribe(&self, ids: &[String]) -> Option<SubscribeChild>;
    fn say(&self, pane: &str, text: &str) -> anyhow::Result<()>;
    /// Interrupt the pane — Escape, the way its own user would.
    fn stop(&self, pane: &str) -> anyhow::Result<()>;
    /// An independent read of these panes' current viewports, taken by
    /// a subscription of its own and given up at `deadline`. `None` when
    /// it could not be taken.
    fn sample(
        &self,
        _ids: &[String],
        _deadline: std::time::Duration,
    ) -> Option<std::collections::BTreeMap<String, Vec<String>>> {
        None
    }
}

/// The real session this TUI runs in.
pub(crate) struct Zellij {
    session: Session,
    repo: PathBuf,
}

impl Zellij {
    /// The server this process runs under, else the one running the
    /// repo's TUI; verified to hold the repo's tab.
    pub(crate) fn find(repo: &Path) -> anyhow::Result<Self> {
        open_zellij::require_zellij()?;
        let session = open_zellij::workspace_session(repo)?;
        let panes = open_zellij::snapshot_panes(session.endpoint()).ok_or_else(|| {
            anyhow::anyhow!("zellij did not answer for session `{session}` — is it running?")
        })?;
        if open_zellij::repo_tab_id(&panes, repo).is_none() {
            anyhow::bail!(
                "session `{session}` does not hold exactly one TUI pane of {}, so its \
                 workspace is unknown",
                repo.display()
            );
        }
        Ok(Self {
            session,
            repo: repo.to_path_buf(),
        })
    }
}

/// Resolve the tool from this repo's current agent table, not from
/// the web server's inherited environment or a pane title.
fn pane_tool(
    repo: &Path,
    table: &[PaneMeta],
    pane: &str,
) -> anyhow::Result<clank_core::vocab::Tool> {
    let label = table
        .iter()
        .find(|entry| entry.id == pane)
        .ok_or_else(|| anyhow::anyhow!("pane `{pane}` is not an agent in this repo"))?
        .label
        .as_str();
    let label = clank_core::ids::AgentLabel::parse(label)?;
    crate::agent_store::resolve_tool(repo, &label)
}

/// Say `text` to `pane` as the pane's own tool, resolved from the agent
/// table as `stop` resolves it: through Codex's app-server when its bound
/// thread takes it, else typed by `terminal`, which refuses any pane the
/// admission rule cannot clear. The text is admitted first.
fn say_with(
    repo: &Path,
    table: &[PaneMeta],
    pane: &str,
    text: &str,
    native: impl FnOnce(&str, &crate::cli::pane_input::Admitted) -> anyhow::Result<bool>,
    terminal: impl FnOnce(
        clank_core::vocab::Tool,
        &crate::cli::pane_input::Admitted,
    ) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    use clank_core::vocab::Tool;
    // Before either route: a host on a replaced build says nothing to an
    // agent, natively or typed.
    let _permit = crate::upgrade::writing()?;
    let tool = pane_tool(repo, table, pane)?;
    let admitted = crate::cli::pane_input::Admitted::new(text);
    if tool == Tool::Codex {
        let label = table
            .iter()
            .find(|entry| entry.id == pane)
            .and_then(|entry| clank_core::ids::AgentLabel::parse(&entry.label).ok());
        if let Some(bound) = label
            .and_then(|label| crate::agent_store::load_agent_config(repo, &label).ok())
            .flatten()
            .and_then(|cfg| cfg.session)
            && bound.tool == Tool::Codex
            && native(bound.id.as_str(), &admitted)?
        {
            return Ok(());
        }
    }
    terminal(tool, &admitted)
}

impl Zellij {
    fn scoped_table(&self, panes: &[open_zellij::ZellijPane]) -> Vec<PaneMeta> {
        scoped_table(
            panes,
            &self.repo,
            &crate::cli::pane_owners::roster_labels(&self.repo),
            crate::cli::pane_owners::owned_labels(&self.repo),
        )
    }
}

/// The repo's agent panes in `panes` that the page shows and may type
/// into: named within the workspace its TUI pane anchors against the
/// roster AND the owned labels, then kept to the roster. Naming against
/// the roster alone would read a removed agent's pane as a current
/// agent's whenever its title also reads as one. A pane that reads as
/// two candidates is nobody's. Empty when the workspace is unresolved.
fn scoped_table(
    panes: &[open_zellij::ZellijPane],
    repo: &Path,
    roster: &std::collections::BTreeSet<String>,
    owned: std::collections::BTreeSet<String>,
) -> Vec<PaneMeta> {
    let mut candidates = roster.clone();
    candidates.extend(owned);
    let Some(ws) = open_zellij::Workspace::anchored(panes, repo, candidates) else {
        return Vec::new();
    };
    open_zellij::agent_pane_table(panes, &ws)
        .into_iter()
        .filter(|meta| roster.contains(&meta.label))
        .collect()
}

impl Panes for Zellij {
    fn session(&self) -> &str {
        self.session.name()
    }
    fn table(&self) -> Option<Vec<PaneMeta>> {
        let panes = open_zellij::snapshot_panes(self.session.endpoint())?;
        Some(self.scoped_table(&panes))
    }
    fn subscribe(&self, ids: &[String]) -> Option<SubscribeChild> {
        open_zellij::subscribe_panes(self.session.endpoint(), ids)
    }
    fn sample(
        &self,
        ids: &[String],
        deadline: std::time::Duration,
    ) -> Option<std::collections::BTreeMap<String, Vec<String>>> {
        sample_initial(
            open_zellij::subscribe_panes(self.session.endpoint(), ids)?,
            ids,
            deadline,
        )
    }
    fn say(&self, pane: &str, text: &str) -> anyhow::Result<()> {
        let panes = open_zellij::snapshot_panes(self.session.endpoint()).ok_or_else(|| {
            anyhow::anyhow!("zellij did not answer for session `{}`", self.session)
        })?;
        let table = self.scoped_table(&panes);
        say_with(
            &self.repo,
            &table,
            pane,
            text,
            codex_chat::send,
            |tool, text| {
                crate::cli::pane_input::say_to_pane_for_tool(
                    self.session.endpoint(),
                    pane,
                    tool,
                    text,
                )
            },
        )
    }
    fn stop(&self, pane: &str) -> anyhow::Result<()> {
        let panes = open_zellij::snapshot_panes(self.session.endpoint()).ok_or_else(|| {
            anyhow::anyhow!("zellij did not answer for session `{}`", self.session)
        })?;
        let table = self.scoped_table(&panes);
        let tool = pane_tool(&self.repo, &table, pane)?;
        crate::cli::pane_input::stop_pane(self.session.endpoint(), pane, tool)
    }
}

#[derive(Default)]
pub(crate) struct ChatStatus {
    following: std::collections::BTreeSet<String>,
    unavailable: std::collections::HashMap<String, String>,
}

/// The status frame: the TUI's own derivation of what the page
/// shows, and the snapshot beneath it. The seed has no tails yet;
/// `observe` replaces it with the actual Chat availability.
pub(crate) fn status_payload(
    _repo: &Path,
    snap: &crate::cli::status::StatusSnapshot,
    chat: &ChatStatus,
) -> serde_json::Value {
    let mut facts = crate::cli::status_tui::web_facts(snap, &chat.following);
    for agent in &mut facts.agents {
        agent.chat_unavailable_reason = chat.unavailable.get(&agent.label).cloned();
    }
    serde_json::json!({
        "facts": facts,
        "snapshot": snap.to_json(),
    })
}

/// How an instance is to run: the tunnel it comes up behind, if any;
/// how long its tasks are given to end; and what the TUI knew when the
/// start was asked for. One thing,
/// because they are answered together at the one place that starts a
/// remote — and eight positional arguments at that call site said
/// less than these four names do (ruthless on 06e16bf).
pub(crate) struct Startup {
    pub(crate) tunnel: Option<tunnel::Start>,
    pub(crate) grace: std::time::Duration,
    /// Published before the server can answer anyone; see `start`.
    pub(crate) seed: serde_json::Value,
    /// The TUI's pane reconciler, reached through its event loop.
    pub(crate) reopen: Option<Reopener>,
    pub(crate) wake: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub(crate) type Reopener =
    Arc<dyn Fn(String) -> tokio::sync::oneshot::Receiver<Result<String, String>> + Send + Sync>;

/// Blocking repo operations remain tracked when their async caller is
/// cancelled, so stopping a bridge can join them before returning.
#[derive(Default)]
pub(crate) struct Blocking {
    handles: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Blocking {
    /// Run `f` off the runtime's threads; `None` if the operation's
    /// thread panicked. The operation runs to completion whether or
    /// not this future is dropped.
    async fn run<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::task::spawn_blocking(move || {
            let _ = tx.send(f());
        });
        {
            let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
            handles.retain(|h| !h.is_finished());
            handles.push(handle);
        }
        rx.await.ok()
    }

    /// Wait for every operation in flight.
    async fn join_all(&self) {
        let handles: Vec<_> =
            std::mem::take(&mut *self.handles.lock().unwrap_or_else(|e| e.into_inner()));
        for h in handles {
            let _ = h.await;
        }
    }
}

/// The tails: the live ones by label, and the retired ones kept until
/// joined — a retired tail is a thread still finishing its poll, and
/// dropping its handle would be exactly the detached work this
/// instance exists not to have.
#[derive(Default)]
struct Tails {
    handles: std::collections::HashMap<String, TailHandle>,
    retired: Vec<TailHandle>,
    next_owner: u64,
}

impl Tails {
    /// Join the retired tails that have finished; the rest wait for
    /// the next pass, or for `stop`.
    fn reap(&mut self) {
        let (done, pending): (Vec<_>, Vec<_>) = self
            .retired
            .drain(..)
            .partition(|h| h.thread.as_ref().is_none_or(|t| t.is_finished()));
        for h in done {
            h.stop_and_join();
        }
        self.retired = pending;
    }
}

/// How long the tasks get to end on their own after the cancel
/// signal before they are aborted: a listing zellij answers slowly,
/// a connection mid-response.
pub(crate) const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// The site, rebuilt off the loop on a thread of its own (the
/// builder's snapshot future is not `Send`, so not a task): one
/// request per rebuild, a burst folded into one build, ended and
/// joined by `stop`.
struct SiteBuilder {
    request: std::sync::mpsc::SyncSender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SiteBuilder {
    fn start(repo: PathBuf, home: Option<PathBuf>) -> Self {
        let (request, requests) = std::sync::mpsc::sync_channel::<()>(1);
        let thread = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            while requests.recv().is_ok() {
                while requests.try_recv().is_ok() {}
                if let Err(e) = rt.block_on(crate::cli::html::generate(
                    &repo,
                    &site_dir(&repo),
                    home.as_deref(),
                    false,
                )) {
                    eprintln!("remote: site build failed: {e:#}");
                }
            }
        });
        Self {
            request,
            thread: Some(thread),
        }
    }
    fn request(&self) {
        let _ = self.request.try_send(());
    }
    fn stop(mut self) {
        let (tx, _) = std::sync::mpsc::sync_channel::<()>(1);
        drop(std::mem::replace(&mut self.request, tx));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The initial viewport of each of `ids` from a fresh subscription,
/// read until all have arrived or `deadline` passes. The child is killed
/// on return either way, which also ends its reader.
fn sample_initial(
    mut child: SubscribeChild,
    ids: &[String],
    deadline: std::time::Duration,
) -> Option<std::collections::BTreeMap<String, Vec<String>>> {
    let stdout = child.take_stdout()?;
    let (tx, rx) = std::sync::mpsc::channel::<SubscribeEvent>();
    let reader = std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(ev) = serde_json::from_str::<SubscribeEvent>(&line)
                && tx.send(ev).is_err()
            {
                break;
            }
        }
    });
    let until = std::time::Instant::now() + deadline;
    let mut seen = std::collections::BTreeMap::new();
    while seen.len() < ids.len() {
        let left = until.saturating_duration_since(std::time::Instant::now());
        let Ok(ev) = rx.recv_timeout(left) else { break };
        if let (Some(id), Some(viewport)) = (ev.pane_id, ev.viewport)
            && ev.is_initial
            && ids.contains(&id)
        {
            seen.insert(id, viewport);
        }
    }
    drop(rx);
    drop(child);
    let _ = reader.join();
    (seen.len() == ids.len()).then_some(seen)
}

/// A viewer's `verify`, as the repo connector received it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verify {
    pub(crate) tab: String,
    pub(crate) id: String,
    pub(crate) panes: Vec<String>,
}

/// The child reading one Zellij pane subscription.
struct Subscription {
    child: SubscribeChild,
    reader: std::thread::JoinHandle<()>,
}

impl Subscription {
    /// Kill the child and wait for the reader.
    fn end(self) {
        drop(self.child);
        let _ = self.reader.join();
    }
}

/// Subscribe to the panes that are alive; the child's lines feed the
/// page until the child ends.
fn subscribe(panes: &dyn Panes, table: &[PaneMeta], feed: &Feed) -> Option<Subscription> {
    subscribe_in(panes, table, feed, feed.next_generation())
}

/// [`subscribe`], in a generation the caller already started.
fn subscribe_in(
    panes: &dyn Panes,
    table: &[PaneMeta],
    feed: &Feed,
    generation: u64,
) -> Option<Subscription> {
    let ids: Vec<String> = table
        .iter()
        .filter(|p| !p.exited)
        .map(|p| p.id.clone())
        .collect();
    let mut child = panes.subscribe(&ids)?;
    let stdout = child.take_stdout()?;
    let feed = feed.clone();
    let reader = std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(ev) = serde_json::from_str::<SubscribeEvent>(&line) {
                feed.event_in(generation, ev);
            }
        }
        // The subscription is over: its panes may be gone, and the
        // supervisor looks at once rather than at its next event.
        feed.relist();
    });
    Some(Subscription { child, reader })
}

/// A transcript being followed for one agent, for one session, from
/// one file. The stop flag is a courtesy; the token the feed's window
/// records for the tail is the guarantee.
struct TailHandle {
    session: String,
    tool: clank_core::vocab::Tool,
    path: PathBuf,
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// Wakes the tail, which otherwise waits for its file to change.
    poke: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TailHandle {
    fn stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self.poke.send(());
    }

    /// Stop the tail and wait for it.
    fn stop_and_join(mut self) {
        self.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for TailHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Which transcript each agent on the roster should be showing: the
/// bound session's file, for harnesses that keep one.
type Wanted = std::collections::HashMap<String, (String, clank_core::vocab::Tool, PathBuf)>;

fn chat_source(
    repo: &Path,
    home: Option<&Path>,
    name: &str,
) -> Result<(String, clank_core::vocab::Tool, PathBuf), &'static str> {
    let label =
        crate::lifecycle::AgentLabel::parse(name).map_err(|_| "This agent's label is invalid.")?;
    let cfg = crate::agent_store::load_agent_config(repo, &label)
        .map_err(|_| "Clank could not read this agent's session binding.")?
        .ok_or("This agent has no session binding.")?;
    let session = cfg.session.ok_or("This agent has no bound session.")?;
    if !transcript::has_adapter(session.tool) {
        return Err("This agent's tool does not provide Chat history.");
    }
    let home = home.ok_or("Clank has no home directory to find Chat history.")?;
    let recorded = session.transcript.as_deref().map(Path::new);
    let path = transcript::transcript_path(session.tool, session.id.as_str(), recorded, repo, home)
        .ok_or("No Chat history file was found for this bound session.")?;
    Ok((session.id.as_str().to_string(), session.tool, path))
}

/// Start a tail for every bound agent whose harness keeps a
/// transcript, stop the tails of agents rebound elsewhere, moved to
/// another file, or gone, and say which agents have one. A new
/// session OR a new path is a new tail: a path recorded after the
/// tail started, or a newer rollout for the same session, would
/// otherwise leave the old tail reading the wrong file (codex on
/// 056a342).
fn reconcile_tails(
    repo: &Path,
    home: Option<&Path>,
    snap: &crate::cli::status::StatusSnapshot,
    feed: &Feed,
    tails: &mut std::collections::HashMap<String, TailHandle>,
    retired: &mut Vec<TailHandle>,
    next_owner: &mut u64,
) -> ChatStatus {
    let mut wanted = Wanted::default();
    let mut chat = ChatStatus::default();
    for a in &snap.agents {
        match chat_source(repo, home, &a.label) {
            Ok(source) => {
                wanted.insert(a.label.clone(), source);
            }
            Err(reason) => {
                chat.unavailable.insert(a.label.clone(), reason.to_string());
            }
        }
    }
    let roster = snap.agents.iter().map(|a| a.label.clone()).collect();
    chat.following = retarget_tails(&wanted, &roster, feed, tails, retired, next_owner);
    chat
}

/// The window an agent shows is decided HERE, synchronously, and the
/// tail only fills what it was handed: every change of scope — a new
/// session, a new file, a binding cleared or moved to a harness
/// without a transcript — is claimed with the next token before any
/// thread runs, so the superseded thread is refused however far into
/// a read it is, and the page never shows a session the roster no
/// longer binds while the new file takes its time to open (codex on
/// eb143e7).
fn retarget_tails(
    wanted: &Wanted,
    roster: &std::collections::BTreeSet<String>,
    feed: &Feed,
    tails: &mut std::collections::HashMap<String, TailHandle>,
    retired: &mut Vec<TailHandle>,
    next_owner: &mut u64,
) -> std::collections::BTreeSet<String> {
    // A tail no longer wanted is stopped, and its handle KEPT for the
    // join: the thread is still finishing its poll (codex on 55db154).
    let gone: Vec<String> = tails
        .iter()
        .filter(|(label, h)| {
            !wanted.get(*label).is_some_and(|(sid, tool, path)| {
                sid == &h.session && tool == &h.tool && path == &h.path
            })
        })
        .map(|(label, _)| label.clone())
        .collect();
    for label in gone {
        if let Some(h) = tails.remove(&label) {
            h.stop();
            retired.push(h);
        }
        if !wanted.contains_key(&label) {
            *next_owner += 1;
            feed.claim_turns(&label, *next_owner, None);
        }
    }
    for (label, (session, tool, path)) in wanted {
        if tails.contains_key(label) {
            continue;
        }
        *next_owner += 1;
        let owner = *next_owner;
        feed.claim_turns(label, owner, Some(session));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (poke, woken) = std::sync::mpsc::channel();
        let (feed, label2, session2, tool2, path2, stop2, poke2) = (
            feed.clone(),
            label.clone(),
            session.clone(),
            *tool,
            path.clone(),
            stop.clone(),
            poke.clone(),
        );
        let thread = std::thread::spawn(move || {
            run_tail(
                feed,
                label2,
                owner,
                session2,
                tool2,
                path2,
                stop2,
                (poke2, woken),
            )
        });
        tails.insert(
            label.clone(),
            TailHandle {
                session: session.clone(),
                tool: *tool,
                path: path.clone(),
                stop,
                poke,
                thread: Some(thread),
            },
        );
    }
    feed.forget_turns_except(roster);
    tails.keys().cloned().collect()
}

/// The last window of a transcript, then only what is appended;
/// bytes enough for the page's last few screens, never the history.
const TAIL_WINDOW: u64 = 4 * 1024 * 1024;

/// Watch for `path` changing: appended to, replaced, or made, along
/// with its directory, which may not exist yet either.
fn watch_transcript(
    path: &Path,
    wake: std::sync::mpsc::Sender<()>,
    fault: crate::fs_notice::Fault,
) -> notify::Result<crate::proc_identity::tree_watch::KernelWatcher> {
    use notify::Watcher as _;
    let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
    let followed = path.to_path_buf();
    let mut watcher = crate::proc_identity::tree_watch::KernelWatcher::new(
        move |p| p == followed,
        transcript_handler(path.to_path_buf(), wake, fault),
    )?;
    watcher.watch(&dir, notify::RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

/// [`watch_transcript`]'s callback, apart from any backend so a test can
/// drive it with what one delivers. The directory appearing is a wake
/// too: the file may have been made with it.
fn transcript_handler(
    path: PathBuf,
    wake: std::sync::mpsc::Sender<()>,
    fault: crate::fs_notice::Fault,
) -> impl FnMut(notify::Result<notify::Event>) + Send + 'static {
    move |res: notify::Result<notify::Event>| {
        use crate::fs_notice::{Notice, notice};
        let ours = match notice(&res) {
            Notice::Read => false,
            Notice::Rescan => true,
            Notice::Changed(paths) => paths
                .iter()
                .any(|p| *p == path || Some(p.as_path()) == path.parent()),
            Notice::Failed(why) => {
                crate::fs_notice::record(&fault, why);
                true
            }
        };
        if ours {
            let _ = wake.send(());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_tail(
    feed: Feed,
    agent: String,
    owner: u64,
    session: String,
    tool: clank_core::vocab::Tool,
    path: PathBuf,
    stop: Arc<std::sync::atomic::AtomicBool>,
    (poke, woken): (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>),
) {
    use std::sync::atomic::Ordering;
    let fault = crate::fs_notice::Fault::default();
    let failed = || match crate::fs_notice::read(&fault) {
        Some(why) => {
            eprintln!(
                "clank: the watch on {agent}'s transcript {} failed: {why}; the remote no \
                 longer follows it",
                path.display()
            );
            true
        }
        None => false,
    };
    let watcher = match watch_transcript(&path, poke.clone(), fault.clone()) {
        Ok(watcher) => watcher,
        Err(e) => {
            eprintln!(
                "clank: cannot watch {}'s transcript {}: {e}; the remote will not follow it",
                agent,
                path.display()
            );
            return;
        }
    };
    // The file may not exist yet: a session that has not spoken.
    let (mut tail, lines) = loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match transcript::Tail::open(&path, TAIL_WINDOW) {
            Ok(opened) => break opened,
            Err(_) => {
                if woken.recv().is_err() {
                    return;
                }
                while woken.try_recv().is_ok() {}
                if failed() {
                    return;
                }
            }
        }
    };
    let _watcher = watcher;
    let mut generation = 1u32;
    feed.turns_reset(
        &agent,
        owner,
        &session,
        generation,
        window_turns(tool, &lines),
    );
    loop {
        if woken.recv().is_err() {
            return;
        }
        while woken.try_recv().is_ok() {}
        if stop.load(Ordering::Relaxed) || failed() {
            return;
        }
        let Ok(polled) = tail.poll() else {
            continue;
        };
        if polled.reset {
            generation += 1;
            feed.turns_reset(
                &agent,
                owner,
                &session,
                generation,
                window_turns(tool, &polled.lines),
            );
            continue;
        }
        for line in &polled.lines {
            for parsed in transcript::parse_at(tool, &line.text, line.offset) {
                match parsed {
                    transcript::Parsed::Turn(t) => {
                        feed.turn(&agent, owner, &session, generation, t)
                    }
                    transcript::Parsed::ToolOutput { id, output, images } => {
                        feed.tool_output(&agent, owner, &session, generation, &id, output, images)
                    }
                }
            }
        }
    }
}

/// A window of lines as turns, with each tool's output already on
/// its call — the batch equivalent of what the live path does one
/// line at a time.
fn window_turns(
    tool: clank_core::vocab::Tool,
    lines: &[transcript::TranscriptLine],
) -> Vec<transcript::Turn> {
    let mut turns: Vec<transcript::Turn> = Vec::new();
    for line in lines {
        for parsed in transcript::parse_at(tool, &line.text, line.offset) {
            match parsed {
                transcript::Parsed::Turn(t) => turns.push(t),
                transcript::Parsed::ToolOutput { id, output, images } => {
                    transcript::apply_tool_output(&mut turns, &id, output, images);
                }
            }
        }
    }
    turns
}

/// The first wait before a dead subscription is restarted; it doubles
/// on each failed restart, up to [`RESTART_MAX`].
const RESTART_MIN: std::time::Duration = std::time::Duration::from_secs(1);
const RESTART_MAX: std::time::Duration = std::time::Duration::from_secs(60);

/// Replace the subscription with one for `table`'s live panes, in a
/// new generation, and end the old one.
fn resubscribe(
    panes: &dyn Panes,
    table: &[PaneMeta],
    feed: &Feed,
    subscription: &Mutex<Option<Subscription>>,
    generation: Option<u64>,
) {
    let generation = generation.unwrap_or_else(|| feed.next_generation());
    let fresh = subscribe_in(panes, table, feed, generation);
    let old = std::mem::replace(
        &mut *subscription.lock().unwrap_or_else(|e| e.into_inner()),
        fresh,
    );
    if let Some(old) = old {
        old.end();
    }
}

/// The repo's pane supervisor. Zellij announces no change to its
/// layout, so the panes are listed again only when something says they
/// may have changed ([`Feed::relist`]): a subscribed pane closing, a
/// subscription ending, the TUI's own listing finding a different set
/// of agents' panes or reopening one, the connector attaching again, and
/// a viewer asking to verify a pane the table does not hold as open. A
/// pane moved, renamed or opened by hand is seen at the next of those.
///
/// It also restarts a subscription whose reader ended, with backoff, and
/// runs the verification samples [`freshness::Freshness`] calls for,
/// answering viewers' `verify` requests; both wait on deadlines, not a
/// clock. Ends on the cancel signal, its subscription with it (`stop`
/// ends that).
async fn pane_supervisor(
    panes: Arc<dyn Panes>,
    feed: Feed,
    subscription: Arc<Mutex<Option<Subscription>>>,
    mut cancelled: tokio::sync::watch::Receiver<bool>,
    blocking: Arc<Blocking>,
    mut verify: tokio::sync::mpsc::UnboundedReceiver<Verify>,
) {
    use std::time::Instant;
    // The runtime's clock, so a paused test drives the schedule.
    let clock = || tokio::time::Instant::now().into_std();
    let mut fresh = freshness::Freshness::default();
    let mut table = feed.table();
    type Sample = Option<Option<std::collections::BTreeMap<String, Vec<String>>>>;
    let mut sampling: Option<std::pin::Pin<Box<dyn std::future::Future<Output = Sample> + Send>>> =
        None;
    let mut backoff = RESTART_MIN;
    let mut restart_at: Option<Instant> = None;
    loop {
        let due = [restart_at, fresh.next_due(&feed.freshness_view())]
            .into_iter()
            .flatten()
            .min()
            .map(tokio::time::Instant::from_std);
        tokio::select! {
            _ = cancelled.changed() => return,
            Some(ask) = verify.recv() => {
                let unknown = ask
                    .panes
                    .iter()
                    .any(|id| !table.iter().any(|p| &p.id == id && !p.exited));
                if unknown {
                    feed.relist();
                }
                fresh.verify(
                    freshness::RequestId { tab: ask.tab, id: ask.id },
                    ask.panes.into_iter().collect(),
                    clock(),
                );
            }
            result = async { sampling.as_mut().expect("guarded").await }, if sampling.is_some() => {
                sampling = None;
                let now = clock();
                match result.flatten() {
                    Some(sampled) => {
                        if let Some(generation) = feed.commit_sample(&mut fresh, sampled, now) {
                            resubscribe(&*panes, &table, &feed, &subscription, Some(generation));
                        }
                    }
                    None => fresh.abandon(),
                }
            }
            _ = feed.relist_asked() => {
                // The listing is waited for, not abandoned: a blocking
                // call cannot be cancelled, so cancel is checked after it.
                let listing = panes.clone();
                let got = blocking.run(move || listing.table()).await;
                if *cancelled.borrow() {
                    return;
                }
                if let Some(Some(listed_now)) = got {
                    let change = feed.panes(listed_now.clone());
                    table = listed_now;
                    if change == TableChange::Membership {
                        resubscribe(&*panes, &table, &feed, &subscription, None);
                    }
                }
            }
            _ = async { tokio::time::sleep_until(due.expect("guarded")).await }, if due.is_some() => {}
        }
        if *cancelled.borrow() {
            return;
        }
        let now = clock();
        let dead = table.iter().any(|p| !p.exited)
            && subscription
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_none_or(|s| s.reader.is_finished());
        match (dead, restart_at) {
            (false, _) => {
                restart_at = None;
                backoff = RESTART_MIN;
            }
            (true, None) => restart_at = Some(now + backoff),
            (true, Some(at)) if now >= at => {
                resubscribe(&*panes, &table, &feed, &subscription, None);
                backoff = (backoff * 2).min(RESTART_MAX);
                restart_at = Some(now + backoff);
            }
            (true, Some(_)) => {}
        }
        if fresh.timed_out(now) {
            sampling = None;
        }
        if sampling.is_none()
            && let Some(start) = fresh.tick(now, &feed.freshness_view())
        {
            let sampler = panes.clone();
            let ids: Vec<String> = start.panes.into_iter().collect();
            let blocking = blocking.clone();
            sampling = Some(Box::pin(async move {
                blocking
                    .run(move || sampler.sample(&ids, freshness::SAMPLE_TIMEOUT))
                    .await
            }));
        }
    }
}

/// Where `clank html` writes the site the host serves under
/// `/html/`.
fn site_dir(repo: &Path) -> PathBuf {
    repo.join(".clank/html")
}

/// Where the site's pages come from: the built site, and the repo
/// for a commit page the site never builds.
struct Pages {
    repo: PathBuf,
    site: PathBuf,
}

/// The site's shapes, and nothing else: a request names a page the
/// builder writes, never a file. Under `plan/`, `queue/` and `stash/`
/// a name is what `PlanKey` accepts — one segment, any letters,
/// spaces and all, no leading dot — decoded from the URL first, since
/// the builder writes the stem verbatim and the link encodes it
/// (codex on 4432a11); under `commit/` a hex sha; and the two files
/// at the root every page links.
fn site_file(rel: &str) -> Option<(&'static str, String)> {
    const HTML: &str = "text/html; charset=utf-8";
    match rel {
        "index.html" => return Some((HTML, rel.to_string())),
        "style.css" => return Some(("text/css; charset=utf-8", rel.to_string())),
        _ => {}
    }
    let (dir, file) = rel.split_once('/')?;
    let file = percent_decode(file)?;
    let stem = file.strip_suffix(".html")?;
    let ok = match dir {
        "commit" => (7..=40).contains(&stem.len()) && stem.chars().all(|c| c.is_ascii_hexdigit()),
        "plan" | "queue" | "stash" => clank_core::ids::PlanKey::parse(stem).is_ok(),
        _ => false,
    };
    ok.then(|| (HTML, format!("{dir}/{file}")))
}

/// One path segment, decoded: `%XX` to bytes, the whole a string.
/// A separator or a NUL that only appears once decoded is refused
/// here rather than passed on as a name.
fn percent_decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let decoded = String::from_utf8(out).ok()?;
    (!decoded.contains(['/', '\\', '\0'])).then_some(decoded)
}

fn site_content(pages: &Pages, rel: &str) -> (u16, String, &'static str) {
    let Some((ctype, file)) = site_file(rel) else {
        return (404, "not a page of this site".into(), "text/plain");
    };
    if let Ok(body) = std::fs::read_to_string(pages.site.join(&file)) {
        return (200, body, ctype);
    }
    // A commit page the site has not written is one of two things: a
    // commit from before adoption, which the site never writes and
    // git still has, rendered here on request; or one the next build
    // will write.
    if let Some(sha) = file
        .strip_prefix("commit/")
        .and_then(|f| f.strip_suffix(".html"))
    {
        return match crate::git_io::resolve_commit(&pages.repo, sha) {
            Some(full) => (
                200,
                crate::cli::html::render_history_commit_page(&pages.repo, &full),
                ctype,
            ),
            None => (404, "no such commit in this repo".into(), "text/plain"),
        };
    }
    (
        404,
        "not built yet: the site is rebuilt after each status refresh; try again shortly".into(),
        "text/plain",
    )
}

/// What a file's bytes are called on disk, chosen from the media
/// type the browser declared — never from anything the client names.
/// An unknown type still lands, as `.bin`: attaching something clank
/// does not recognise should fail at the agent that reads it, not
/// here, and a name clank invented cannot be a path or a program.
fn extension_for(media_type: &str) -> &'static str {
    match media_type.split(';').next().unwrap_or("").trim() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/markdown" => "md",
        "text/csv" => "csv",
        "application/json" => "json",
        _ => "bin",
    }
}

/// The most attachments kept. Every one is a file nobody deletes, on
/// a machine that runs for weeks — Claude Code's own image cache is
/// the cautionary example, keeping every pasted image forever. A
/// count, not an age: it needs no clock to enforce and none to test.
const ATTACHMENTS_KEPT: usize = 20;

/// Delete all but the newest [`ATTACHMENTS_KEPT`] files in `dir`.
fn prune_attachments(dir: &Path) {
    let Ok(_permit) = crate::upgrade::writing() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file().then_some(())?;
            Some((meta.modified().ok()?, e.path()))
        })
        .collect();
    if files.len() <= ATTACHMENTS_KEPT {
        return;
    }
    files.sort_by_key(|(at, _)| *at);
    for (_, path) in &files[..files.len() - ATTACHMENTS_KEPT] {
        let _ = std::fs::remove_file(path);
    }
}

/// What the page asks of the queue. The handle is a FILE's stem, not
/// a path: what a browser sends names an entry, it does not point at
/// one, and the server resolves it against the queue as it is now
/// (the-queue-is-workable-from-the-phone).
#[derive(serde::Deserialize)]
#[serde(tag = "do", rename_all = "lowercase")]
enum QueueAsk {
    Preview {
        body: String,
    },
    Add {
        name: String,
        priority: u16,
        body: String,
    },
    Open {
        handle: String,
    },
    Save {
        handle: String,
        original: String,
        revision: String,
        body: String,
    },
    Promote {
        handle: String,
    },
    Unqueue {
        handle: String,
    },
    Priority {
        handle: String,
        priority: u16,
    },
}

/// Create, edit, promote, unqueue or reprioritise a queued plan — the primitives
/// the pane's own page calls, so promoting means one thing on both
/// surfaces. Blocking work (the queue's files, and a promote's commit)
/// runs off the loop, as a say does.
fn perform_queue_action(repo: &Path, ask: QueueAsk) -> anyhow::Result<Option<serde_json::Value>> {
    use crate::cli::queue;
    let location = |path: &Path| -> anyhow::Result<serde_json::Value> {
        let handle = path
            .file_stem()
            .ok_or_else(|| anyhow::anyhow!("queued plan has no file stem"))?
            .to_string_lossy()
            .into_owned();
        Ok(serde_json::json!({
            "handle": handle,
            "page": crate::cli::html::queue_page_for_stem(&handle),
        }))
    };
    match ask {
        QueueAsk::Preview { body } => Ok(Some(serde_json::json!({
            "html": crate::cli::html::render_markdown(&body)
        }))),
        QueueAsk::Add {
            name,
            priority,
            body,
        } => {
            let path = queue::add(
                repo,
                &name,
                priority,
                queue::BodySource {
                    raw: body,
                    kind: queue::BodySourceKind::Inline,
                },
            )?;
            Ok(Some(location(&path)?))
        }
        QueueAsk::Open { handle } => {
            let (entry, body, revision) = queue::read_entry(repo, &handle)?;
            Ok(Some(
                serde_json::json!({"handle":handle,"name":entry.name,"priority":entry.priority,"body":body,"revision":revision}),
            ))
        }
        QueueAsk::Save {
            handle,
            original,
            revision,
            body,
        } => {
            queue::save_entry(repo, &handle, &original, &revision, body)?;
            Ok(None)
        }
        QueueAsk::Promote { handle } => {
            let entry = queue::entry_by_stem(repo, &handle)?;
            queue::promote_entry(repo, &entry)?;
            Ok(None)
        }
        QueueAsk::Unqueue { handle } => {
            let entry = queue::entry_by_stem(repo, &handle)?;
            queue::unqueue_entry(repo, &entry)?;
            Ok(None)
        }
        QueueAsk::Priority { handle, priority } => {
            let entry = queue::entry_by_stem(repo, &handle)?;
            let path = queue::set_priority_of(repo, &entry, priority)?;
            Ok(Some(location(&path)?))
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(tag = "do", rename_all = "lowercase")]
enum StashAsk {
    Open { stem: String },
    Restore { stem: String, revision: String },
}

/// Inspect the TUI's stashed-plan source or restore the exact stash the
/// page opened. Restoring replays commits, so it runs on a blocking
/// worker rather than holding the web server's event loop.
fn perform_stash_action(
    repo: &Path,
    ask: StashAsk,
    runtime: &tokio::runtime::Handle,
) -> anyhow::Result<Option<serde_json::Value>> {
    match ask {
        StashAsk::Open { stem } => {
            let (record, markdown, revision) = crate::cli::stash::read_stashed(repo, &stem)?;
            Ok(Some(serde_json::json!({
                "stem": stem, "body": markdown, "revision": revision,
                "commits": record.shas.len(), "waiting_for": record.waiting_for,
            })))
        }
        StashAsk::Restore { stem, revision } => {
            runtime.block_on(crate::cli::stash::run_pop_checked(
                Some(repo),
                Some(&stem),
                Some(&revision),
            ))?;
            Ok(None)
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
enum AgentAsk {
    Add {
        label: String,
        tool: Option<clank_core::vocab::Tool>,
        review: crate::cli::teams_config::ReviewKind,
    },
    Review {
        label: String,
        review: crate::cli::teams_config::ReviewKind,
    },
    Promote {
        label: String,
    },
    Swap {
        label: String,
        into: String,
    },
    Remove {
        label: String,
    },
    ClearSession {
        label: String,
        session: String,
    },
    AutoMode {
        label: String,
        mode: clank_core::vocab::AutoMode,
    },
}

/// Repo-roster mutations go through the same cores as the TUI. None
/// of these operations binds a session or starts a pane; the TUI's
/// reconciler observes the config write and owns the resulting pane.
fn perform_agent_action(repo: &Path, home: Option<&Path>, ask: AgentAsk) -> anyhow::Result<()> {
    use crate::cli::agent;
    use clank_core::ids::AgentLabel;
    let parse = |raw: &str| {
        AgentLabel::parse(raw).map_err(|e| anyhow::anyhow!("invalid agent label `{raw}`: {e}"))
    };
    match ask {
        AgentAsk::Add {
            label,
            tool,
            review,
        } => {
            let label = parse(&label)?;
            let role: crate::cli::teams_config::RosterRole = review.into();
            match tool {
                Some(tool) => agent::add_repo_roster_agent(
                    repo,
                    &label,
                    crate::cli::teams_config::AgentDescription {
                        tool,
                        launch: None,
                        initial_prompt: None,
                    },
                    role,
                ),
                None => agent::add_repo_roster_agent_by_name(repo, home, &label, role),
            }
        }
        AgentAsk::Review { label, review } => agent::set_repo_review(repo, &parse(&label)?, review),
        AgentAsk::Promote { label } => agent::set_repo_master(repo, &parse(&label)?),
        AgentAsk::Swap { label, into } => {
            agent::swap_repo_agent(repo, home, &parse(&label)?, &parse(&into)?)
        }
        AgentAsk::Remove { label } => agent::remove_repo_reviewer(repo, &parse(&label)?),
        AgentAsk::ClearSession { label, session } => {
            let label = parse(&label)?;
            let roster = crate::agent_store::load_repo_config_required(repo)?;
            if !roster.agents.contains_key(&label) {
                anyhow::bail!("`{label}` is no longer on the roster");
            }
            let id = clank_core::ids::SessionId::parse(&session)
                .map_err(|e| anyhow::anyhow!("invalid session id `{session}`: {e}"))?;
            if !crate::agent_store::release_binding(repo, &label, &id)? {
                anyhow::bail!(
                    "`{label}` is no longer bound to {session}: its binding changed after the confirm showed it"
                );
            }
            Ok(())
        }
        AgentAsk::AutoMode { label, mode } => {
            let label = parse(&label)?;
            let roster = crate::agent_store::load_repo_config_required(repo)?;
            if !roster.agents.contains_key(&label) {
                anyhow::bail!("`{label}` is no longer on the roster");
            }
            crate::agent_store::set_auto_mode(repo, &label, mode)
        }
    }
}

fn library_candidates(home: Option<&Path>) -> anyhow::Result<serde_json::Value> {
    let home = home.ok_or_else(|| anyhow::anyhow!("no home directory for the agent library"))?;
    let cfg = crate::cli::team::read_user_config(home)?;
    Ok(serde_json::Value::Array(
        cfg.agents
            .into_iter()
            .map(|(label, desc)| {
                serde_json::json!({"label": label.as_str(), "tool": desc.tool.as_str()})
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    /// What the remote asks of a TUI kept running on a replaced build is
    /// refused where the change would be written, with the reason the
    /// TUI's own bar gives, and nothing is changed.
    #[test]
    fn a_stale_host_refuses_a_remote_ask_and_says_why() {
        use crate::upgrade::tests::{AsProcess, Installed};
        let repo = crate::cli::status_tui::fixtures::detail_repo();
        let label = clank_core::ids::AgentLabel::parse("codex").unwrap();
        let ask = || super::AgentAsk::AutoMode {
            label: "codex".into(),
            mode: clank_core::vocab::AutoMode::On,
        };
        let armed = || {
            crate::agent_store::load_agent_config(repo.path(), &label)
                .unwrap()
                .and_then(|cfg| cfg.auto_mode)
        };
        let installed = Installed::current();
        let authority = installed.authority();
        let _as = AsProcess::of(&authority);

        installed.replace(1);
        let why = super::perform_agent_action(repo.path(), None, ask()).unwrap_err();
        assert!(
            format!("{why:#}").contains(crate::upgrade::STALE_BADGE),
            "{why:#}"
        );
        assert_eq!(armed(), None);

        installed.install(crate::build_stamp::running());
        super::perform_agent_action(repo.path(), None, ask()).unwrap();
        assert_eq!(armed(), Some(clank_core::vocab::AutoMode::On));
    }

    /// The bridge's `say` types as the pane's OWN tool, resolved from the
    /// agent table (it once typed every non-Codex pane as Claude), and hands
    /// the terminal and Codex's native path the ADMITTED text: a leading `!`
    /// encoded, a CR made an LF.
    #[test]
    fn say_uses_the_panes_own_tool_and_admitted_text() {
        use clank_core::vocab::Tool;
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"claude":{"tool":"claude","role":"master"},"grok":{"tool":"grok","role":"commit"},"codex":{"tool":"codex","role":"commit"}}}"#,
        )
        .unwrap();
        crate::agent_store::bind_session_to_agent(
            repo.path(),
            &clank_core::ids::AgentLabel::parse("codex").unwrap(),
            Tool::Codex,
            &clank_core::SessionId::parse("thread-0001").unwrap(),
        )
        .unwrap();
        let meta = |id: &str, label: &str| PaneMeta {
            id: id.into(),
            label: label.into(),
            columns: 80,
            rows: 24,
            exited: false,
        };
        let table = [meta("1", "claude"), meta("2", "grok"), meta("3", "codex")];
        for (pane, tool) in [("1", Tool::Claude), ("2", Tool::Grok)] {
            let mut typed = None;
            say_with(
                repo.path(),
                &table,
                pane,
                "!git push\rnow",
                |_, _| panic!("only Codex has a native path"),
                |tool, text| {
                    typed = Some((tool, text.as_str().to_string()));
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(typed, Some((tool, "\u{b7}!git push\nnow".to_string())));
        }
        let mut native = None;
        say_with(
            repo.path(),
            &table,
            "3",
            "!x",
            |thread, text| {
                native = Some((thread.to_string(), text.as_str().to_string()));
                Ok(true)
            },
            |_, _| panic!("the app-server took it"),
        )
        .unwrap();
        assert_eq!(native, Some(("thread-0001".into(), "\u{b7}!x".into())));
        assert!(
            say_with(
                repo.path(),
                &table,
                "9",
                "x",
                |_, _| Ok(false),
                |_, _| Ok(())
            )
            .is_err(),
            "a pane that is not an agent here"
        );

        // A host kept running on a replaced build says nothing to any
        // agent: neither the native route nor the typed one is reached,
        // and the remote is told why in the bar's own words.
        use crate::upgrade::tests::{AsProcess, Installed};
        let installed = Installed::current();
        let authority = installed.authority();
        let _as = AsProcess::of(&authority);
        installed.replace(1);
        let (mut natively, mut by_typing) = (0, 0);
        for pane in ["3", "1"] {
            let refused = say_with(
                repo.path(),
                &table,
                pane,
                "carry on",
                |_, _| {
                    natively += 1;
                    Ok(true)
                },
                |_, _| {
                    by_typing += 1;
                    Ok(())
                },
            )
            .unwrap_err();
            assert!(
                format!("{refused:#}").contains(crate::upgrade::STALE_BADGE),
                "{refused:#}"
            );
        }
        assert_eq!((natively, by_typing), (0, 0));
        let endpoint = crate::cli::zellij_endpoint::Endpoint::fixture_here("/nowhere");
        let stopped =
            crate::cli::pane_input::stop_pane(&endpoint, "terminal_1", Tool::Claude).unwrap_err();
        assert!(
            format!("{stopped:#}").contains(crate::upgrade::STALE_BADGE),
            "{stopped:#}"
        );
    }

    /// The tail reading its transcript is not a change to it, and a
    /// failed watch is recorded for the tail to stop on.
    #[test]
    fn a_transcripts_reads_wake_nothing_and_its_failure_is_recorded() {
        use crate::fs_notice::tests::{event, opened};
        let path = std::path::Path::new("/projects/p/session.jsonl");
        let (tx, rx) = std::sync::mpsc::channel();
        let fault = crate::fs_notice::Fault::default();
        let mut handle = transcript_handler(path.to_path_buf(), tx, fault.clone());
        handle(opened(path));
        assert!(rx.try_recv().is_err(), "its own read woke it");
        handle(event(
            notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            path,
        ));
        assert!(rx.try_recv().is_ok(), "an append wakes it");
        handle(Err(notify::Error::generic("the stream died")));
        assert!(rx.try_recv().is_ok());
        assert_eq!(
            crate::fs_notice::read(&fault).as_deref(),
            Some("the stream died")
        );
    }

    /// A transcript is followed by its directory's events: an append
    /// wakes the tail, nothing else does, and a file whose directory
    /// does not exist yet is heard when it is made.
    #[test]
    fn a_transcript_wakes_its_tail_only_when_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dunce::canonicalize(dir.path())
            .unwrap()
            .join("session.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let _watch = watch_transcript(&path, tx, Default::default()).unwrap();
        while rx
            .recv_timeout(std::time::Duration::from_millis(500))
            .is_ok()
        {}
        std::fs::write(dir.path().join("other.jsonl"), "{}\n").unwrap();
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(1)).is_err(),
            "another session's file"
        );
        std::io::Write::write_all(
            &mut std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
            b"{}\n",
        )
        .unwrap();
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "an append"
        );
        while rx
            .recv_timeout(std::time::Duration::from_millis(300))
            .is_ok()
        {}
        let replacement = path.with_extension("tmp");
        std::fs::write(&replacement, "{}\n").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "a rename over it"
        );
        while rx
            .recv_timeout(std::time::Duration::from_millis(300))
            .is_ok()
        {}
        std::io::Write::write_all(
            &mut std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
            b"{}\n",
        )
        .unwrap();
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "an append to the file renamed in"
        );

        let later = dir.path().join("project/new.jsonl");
        let (tx, rx) = std::sync::mpsc::channel();
        let _watch = watch_transcript(&later, tx, Default::default()).unwrap();
        while rx
            .recv_timeout(std::time::Duration::from_millis(500))
            .is_ok()
        {}
        std::fs::create_dir_all(later.parent().unwrap()).unwrap();
        std::fs::write(&later, "{}\n").unwrap();
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "its creation"
        );
    }

    /// `painter` is on the roster and `💤 painter` is recorded as owned but
    /// off it, so a pane titled `💤 painter` may be either. It is not
    /// published as painter, and neither `say` nor `stop` will treat it as
    /// an agent to type into; claude's pane beside it still is one.
    #[test]
    fn a_pane_that_may_be_a_removed_agents_is_no_input_target() {
        use super::*;
        let repo = tempfile::tempdir().unwrap();
        let panes: Vec<open_zellij::ZellijPane> = serde_json::from_str(&format!(
            r#"[{{"id":1,"title":"","terminal_command":"clank tui --repo {}","tab_id":0}},
                {{"id":2,"title":"🔨 claude","tab_id":0}},
                {{"id":7,"title":"💤 painter","tab_id":0}}]"#,
            repo.path().display()
        ))
        .unwrap();
        let roster: std::collections::BTreeSet<String> =
            ["claude", "painter"].map(String::from).into();
        let table = scoped_table(
            &panes,
            repo.path(),
            &roster,
            ["💤 painter".to_string()].into(),
        );
        let published: Vec<(&str, &str)> = table
            .iter()
            .map(|m| (m.id.as_str(), m.label.as_str()))
            .collect();
        assert_eq!(published, [("terminal_2", "claude")]);
        assert!(
            pane_tool(repo.path(), &table, "terminal_7").is_err(),
            "stop"
        );
        assert!(
            say_with(
                repo.path(),
                &table,
                "terminal_7",
                "x",
                |_, _| panic!("not an input target"),
                |_, _| panic!("not an input target"),
            )
            .is_err(),
            "say"
        );
    }

    mod supervisor {
        use super::super::*;
        use std::collections::{BTreeMap, VecDeque};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        /// Panes whose subscription is a test process and whose samples
        /// are scripted.
        struct Scripted {
            alive: bool,
            samples: Mutex<VecDeque<&'static str>>,
            /// When each subscription was started, on the paused clock.
            subscribed: Mutex<Vec<tokio::time::Instant>>,
            sampled: AtomicUsize,
            listed: AtomicUsize,
        }

        impl Scripted {
            fn new(alive: bool, samples: &[&'static str]) -> Arc<Self> {
                Arc::new(Self {
                    alive,
                    samples: Mutex::new(samples.iter().copied().collect()),
                    subscribed: Mutex::new(Vec::new()),
                    sampled: AtomicUsize::new(0),
                    listed: AtomicUsize::new(0),
                })
            }

            fn subscriptions(&self) -> usize {
                self.subscribed.lock().unwrap().len()
            }
        }

        impl Panes for Scripted {
            fn session(&self) -> &str {
                "test"
            }
            fn table(&self) -> Option<Vec<PaneMeta>> {
                self.listed.fetch_add(1, Ordering::SeqCst);
                Some(table())
            }
            fn subscribe(&self, _: &[String]) -> Option<SubscribeChild> {
                self.subscribed
                    .lock()
                    .unwrap()
                    .push(tokio::time::Instant::now());
                let script = if self.alive {
                    "exec sleep 300"
                } else {
                    "exit 0"
                };
                let child = crate::proc_run::start(
                    crate::proc_run::Spec::new("/bin/sh")
                        .args(["-c", script])
                        .stdout(crate::proc_run::Out::Piped),
                )
                .ok()?;
                Some(SubscribeChild::from_child(child))
            }
            fn say(&self, _: &str, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn stop(&self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn sample(&self, _: &[String], _: Duration) -> Option<BTreeMap<String, Vec<String>>> {
                self.sampled.fetch_add(1, Ordering::SeqCst);
                let screen = self.samples.lock().unwrap().pop_front()?;
                Some([("p".to_string(), vec![screen.to_string()])].into())
            }
        }

        fn table() -> Vec<PaneMeta> {
            vec![PaneMeta {
                id: "p".into(),
                label: "codex".into(),
                columns: 80,
                rows: 24,
                exited: false,
            }]
        }

        fn frame(feed: &Feed, screen: &str) {
            let ev: SubscribeEvent = serde_json::from_value(serde_json::json!({
                "event": "pane_update", "is_initial": false, "pane_id": "p",
                "viewport": [screen], "scrollback": null,
            }))
            .unwrap();
            feed.event_in(feed.freshness_view().generation, ev);
        }

        struct Running {
            feed: Feed,
            verify: tokio::sync::mpsc::UnboundedSender<Verify>,
            cancel: tokio::sync::watch::Sender<bool>,
            task: tokio::task::JoinHandle<()>,
            subscription: Arc<Mutex<Option<Subscription>>>,
        }

        fn supervise(panes: Arc<Scripted>) -> Running {
            let feed = Feed::new();
            feed.panes(table());
            let subscription = Arc::new(Mutex::new(subscribe(&*panes, &table(), &feed)));
            frame(&feed, "a");
            let (cancel, cancelled) = tokio::sync::watch::channel(false);
            let (verify, rx) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::spawn(pane_supervisor(
                panes,
                feed.clone(),
                subscription.clone(),
                cancelled,
                Arc::new(Blocking::default()),
                rx,
            ));
            Running {
                feed,
                verify,
                cancel,
                task,
                subscription,
            }
        }

        impl Running {
            fn ask(&self, id: &str) {
                self.verify
                    .send(Verify {
                        tab: "t".into(),
                        id: id.into(),
                        panes: vec!["p".into()],
                    })
                    .unwrap();
            }
            async fn stop(self) {
                let _ = self.cancel.send(true);
                let _ = self.task.await;
                let subscription = self.subscription.lock().unwrap().take();
                if let Some(s) = subscription {
                    tokio::task::spawn_blocking(move || s.end()).await.unwrap();
                }
            }
        }

        fn shown(feed: &Feed) -> (Vec<String>, u64) {
            let v = feed.freshness_view();
            (v.viewports["p"].clone(), v.revisions["p"])
        }

        /// `secs` of the paused clock, giving the real threads (a
        /// subscription's reader noticing EOF) a moment at each step.
        async fn pass(secs: u64) {
            for _ in 0..secs * 4 {
                tokio::time::sleep(Duration::from_millis(250)).await;
                tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(3)))
                    .await
                    .unwrap();
            }
        }

        #[tokio::test(start_paused = true)]
        async fn an_alive_but_stalled_subscription_is_proven_by_two_samples_and_replaced() {
            let panes = Scripted::new(true, &["b", "c"]);
            let run = supervise(panes.clone());
            run.ask("r1");
            pass(1).await;
            assert_eq!(
                shown(&run.feed),
                (vec!["a".to_string()], 1),
                "one mismatch proves nothing"
            );
            pass(3).await;
            assert_eq!(
                shown(&run.feed),
                (vec!["c".to_string()], 2),
                "the confirming sample is published"
            );
            assert_eq!(panes.subscriptions(), 2, "and the subscription restarted");
            run.stop().await;
        }

        #[tokio::test(start_paused = true)]
        async fn a_quiet_healthy_pane_is_confirmed_without_churn() {
            let panes = Scripted::new(true, &["a", "a"]);
            let run = supervise(panes.clone());
            run.ask("r1");
            pass(2).await;
            run.ask("r2");
            pass(20).await;
            assert_eq!(
                panes.sampled.load(Ordering::SeqCst),
                2,
                "one sample per window"
            );
            assert_eq!(panes.subscriptions(), 1, "never restarted");
            assert_eq!(shown(&run.feed), (vec!["a".to_string()], 1));
            run.stop().await;
        }

        #[tokio::test(start_paused = true)]
        async fn a_changing_healthy_pane_is_not_restarted() {
            let panes = Scripted::new(true, &["b", "b"]);
            let run = supervise(panes.clone());
            run.ask("r1");
            pass(1).await;
            frame(&run.feed, "b");
            pass(3).await;
            assert_eq!(panes.subscriptions(), 1);
            assert_eq!(shown(&run.feed), (vec!["b".to_string()], 2));
            run.stop().await;
        }

        /// The paused clock's step while the supervisor restarts.
        const STEP: Duration = Duration::from_millis(250);

        impl Running {
            /// Block until the live subscription's reader has seen its
            /// child's EOF. The reader is a real thread the paused clock
            /// does not wait for: a tick that outran it read a dying
            /// subscription as healthy and reset the backoff. Blocking the
            /// runtime thread holds both the clock and the supervisor.
            fn died(&self) {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                // A JoinHandle the supervisor owns can be asked, not awaited.
                while !self
                    .subscription
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_none_or(|s| s.reader.is_finished())
                {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the scripted subscription never ended"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
            }

            /// Step the paused clock until the supervisor subscribes again,
            /// each step only once the subscription it holds is dead.
            async fn resubscribed(&self, panes: &Scripted) {
                let before = panes.subscriptions();
                while panes.subscriptions() == before {
                    self.died();
                    tokio::time::sleep(STEP).await;
                }
            }
        }

        #[tokio::test(start_paused = true)]
        async fn a_dead_subscription_is_restarted_with_backoff() {
            let panes = Scripted::new(false, &[]);
            let run = supervise(panes.clone());
            for _ in 0..5 {
                run.resubscribed(&panes).await;
            }
            let at = panes.subscribed.lock().unwrap().clone();
            let gaps: Vec<Duration> = at.windows(2).map(|w| w[1] - w[0]).collect();
            assert!(
                gaps[0].abs_diff(Duration::from_secs(1)) <= STEP,
                "the first restart waits about a second: {gaps:?}"
            );
            for pair in gaps.windows(2) {
                assert!(
                    pair[1] > pair[0] && pair[1].abs_diff(pair[0] * 2) <= STEP,
                    "each wait doubles the one before: {gaps:?}"
                );
            }
            run.stop().await;
        }

        /// Zellij announces no layout change, and nothing here invents
        /// one: an idle supervisor with a healthy subscription never
        /// lists, however long it runs.
        #[tokio::test(start_paused = true)]
        async fn an_idle_supervisor_never_lists() {
            let panes = Scripted::new(true, &[]);
            let run = supervise(panes.clone());
            pass(120).await;
            assert_eq!(panes.listed.load(Ordering::SeqCst), 0);
            run.stop().await;
        }

        /// Each thing that says the panes may have changed lists them
        /// once: a subscribed pane closing, the TUI's word, and a viewer
        /// asking after a pane the table does not hold as open. A viewer
        /// asking after an open one does not.
        #[tokio::test(start_paused = true)]
        async fn each_announced_change_lists_once() {
            let panes = Scripted::new(true, &[]);
            let run = supervise(panes.clone());
            let listed = || panes.listed.load(Ordering::SeqCst);
            let closed: SubscribeEvent = serde_json::from_value(serde_json::json!({
                "event": "pane_closed", "pane_id": "p",
            }))
            .unwrap();
            run.feed
                .event_in(run.feed.freshness_view().generation, closed);
            pass(1).await;
            assert_eq!(listed(), 1, "a pane closing");
            run.feed.relist();
            pass(1).await;
            assert_eq!(listed(), 2, "the TUI's word");
            run.ask("open");
            pass(1).await;
            assert_eq!(listed(), 2, "a viewer asking after an open pane");
            run.verify
                .send(Verify {
                    tab: "t".into(),
                    id: "gone".into(),
                    panes: vec!["q".into()],
                })
                .unwrap();
            pass(1).await;
            assert_eq!(listed(), 3, "a viewer asking after a pane it does not hold");
            run.stop().await;
        }

        #[tokio::test(start_paused = true)]
        async fn nothing_is_sampled_without_demand() {
            let panes = Scripted::new(true, &["a"]);
            let run = supervise(panes.clone());
            pass(60).await;
            assert_eq!(panes.sampled.load(Ordering::SeqCst), 0);
            run.stop().await;
        }
    }

    #[test]
    fn replay_and_incremental_task_observations_have_the_same_turns_and_ids() {
        use clank_core::vocab::Tool;
        let records = [
            serde_json::json!({"type":"assistant","tool_calls":[
                {"id":"register","name":"run_terminal_command","arguments":{"command":"clank wait"}},
                {"id":"observe","name":"get_command_or_subagent_output","arguments":{"task_ids":["fixture"]}}]}),
            // A background result can arrive before its registration result.
            serde_json::json!({"type":"tool_result","tool_call_id":"observe","content":
                "=== Task fixture ===\nCommand: clank wait\nStatus: completed\nDuration: 1s\nExit Code: 0\nOutput File: /tmp/fixture.log\n\nactual output\n"}),
            serde_json::json!({"type":"tool_result","tool_call_id":"register","content":
                "<task-id>fixture</task-id>\n<task-type>bash</task-type>\n<output-file>/tmp/fixture.log</output-file>\n<status>running</status>\n<summary>Background task started</summary>"}),
        ];
        let lines: Vec<_> = records
            .iter()
            .enumerate()
            .map(|(at, record)| transcript::TranscriptLine {
                offset: at as u64 * 100,
                text: record.to_string(),
            })
            .collect();
        let replay = super::window_turns(Tool::Grok, &lines);
        let feed = Feed::new();
        feed.claim_turns("grok", 1, Some("fixture-session"));
        feed.turns_reset("grok", 1, "fixture-session", 1, vec![]);
        for line in &lines {
            for item in transcript::parse_at(Tool::Grok, &line.text, line.offset) {
                match item {
                    transcript::Parsed::Turn(turn) => {
                        feed.turn("grok", 1, "fixture-session", 1, turn)
                    }
                    transcript::Parsed::ToolOutput { id, output, images } => {
                        feed.tool_output("grok", 1, "fixture-session", 1, &id, output, images)
                    }
                }
            }
        }
        assert_eq!(feed.snapshot().turns["grok"].turns, replay);
        assert!(
            matches!(&replay[0].body, transcript::Body::Tool { tasks, .. }
            if tasks[0].status == "completed" && tasks[0].output.as_deref() == Some("actual output"))
        );
        assert_eq!(replay[0].id, "grok-tool:register");
    }

    use super::*;

    #[test]
    fn queue_page_actions_return_exact_new_location_and_safe_preview() {
        let repo = tempfile::tempdir().unwrap();
        let preview = perform_queue_action(
            repo.path(),
            QueueAsk::Preview {
                body: "# Plan\n<script>alert(1)</script>".into(),
            },
        )
        .unwrap()
        .unwrap();
        let rendered = preview["html"].as_str().unwrap();
        assert!(rendered.contains("Plan"));
        assert!(!rendered.contains("<script>"));

        let added = perform_queue_action(
            repo.path(),
            QueueAsk::Add {
                name: "sample".into(),
                priority: 500,
                body: "# sample\n\nThe work.\n".into(),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(added["handle"], "500-sample");
        assert_eq!(added["page"], "queue/500-sample.html");

        let moved = perform_queue_action(
            repo.path(),
            QueueAsk::Priority {
                handle: "500-sample".into(),
                priority: 100,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(moved["handle"], "100-sample");
        assert_eq!(moved["page"], "queue/100-sample.html");
        assert!(
            perform_queue_action(
                repo.path(),
                QueueAsk::Open {
                    handle: "500-sample".into(),
                },
            )
            .is_err()
        );
    }

    #[test]
    fn site_resources_are_named_pages_and_cannot_escape_the_repo() {
        for path in [
            "index.html",
            "style.css",
            "plan/foo.html",
            "plan/fix%20UI.html",
            "queue/500-foo.html",
            "stash/foo.html",
            "commit/abc1234.html",
        ] {
            assert!(site_file(path).is_some(), "{path}");
        }
        assert_eq!(
            site_file("plan/r%C3%A9sum%C3%A9.html").unwrap().1,
            "plan/résumé.html"
        );
        for path in [
            "",
            "plan/",
            "plan/.hidden.html",
            "plan/../style.css",
            "plan/..%2Fstyle.css",
            "plan/%2E%2E.html",
            "plan/a%5Cb.html",
            "plan/a%00b.html",
            "plan/%ZZ.html",
            "commit/abc.html",
            "other/foo.html",
            "/etc/passwd",
        ] {
            assert!(site_file(path).is_none(), "{path}");
        }
    }

    #[test]
    fn pane_interrupt_resolves_the_target_agents_tool() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"grok":{"tool":"grok","role":"master"},"claude":{"tool":"claude","role":"commit"}}}"#,
        )
        .unwrap();
        let table = ["grok", "claude"]
            .into_iter()
            .enumerate()
            .map(|(index, label)| PaneMeta {
                id: format!("terminal_{index}"),
                label: label.into(),
                columns: 80,
                rows: 24,
                exited: false,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            pane_tool(repo.path(), &table, "terminal_0").unwrap(),
            clank_core::vocab::Tool::Grok
        );
        assert_eq!(
            pane_tool(repo.path(), &table, "terminal_1").unwrap(),
            clank_core::vocab::Tool::Claude
        );
        assert!(pane_tool(repo.path(), &table, "terminal_2").is_err());
    }
}
