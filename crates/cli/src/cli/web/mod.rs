//! The remote: one repo's agents in a browser, one pane at a time,
//! served from inside `clank tui` as one owned [`Instance`].
//!
//! `zellij web` shares a session at ONE geometry, sized to its
//! smallest client, so a phone gets the desktop's tab or shrinks the
//! desktop. This subscribes to each agent pane on its own instead:
//! the viewport arrives independent of the layout, and the page shows
//! one agent at a time. Terminals do not reflow, so a pane arrives at
//! its desktop size; the page scrolls and pinches around it.
//!
//! Localhost, no authentication yet (the-tui-mints-the-way-in).

mod feed;
mod transcript;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Bytes, Frame};

use crate::cli::open_zellij::{self, PaneMeta, SubscribeChild};
use feed::{Feed, SubscribeEvent, TableChange};

const PAGE: &str = include_str!("page.html");

/// Which session to look in. `--session` wins; else the one this
/// process runs inside; else the name `clank open` would have used.
/// Only a preference: the caller verifies the listing, because
/// `open_one` adds a repo's tab to whatever session the caller was
/// in, and the convention is where penlock's panes were NOT.
pub(crate) fn choose_session(explicit: Option<&str>, current: Option<&str>, repo: &Path) -> String {
    explicit
        .or(current)
        .map(str::to_string)
        .unwrap_or_else(|| open_zellij::repo_session_name(repo))
}

/// How the page says something to a pane; the production one runs
/// zellij, tests record.
type Sayer = Arc<dyn Fn(&str, &str) -> anyhow::Result<()> + Send + Sync>;

/// What the server needs of zellij, behind a trait so an instance
/// runs in a test without one: the session, its panes, a
/// subscription to some of them, and a way to type into one.
pub(crate) trait Panes: Send + Sync + 'static {
    fn session(&self) -> &str;
    /// This repo's agent panes as the session shows them now; `None`
    /// when zellij does not answer.
    fn table(&self) -> Option<Vec<PaneMeta>>;
    fn subscribe(&self, ids: &[String]) -> Option<SubscribeChild>;
    fn say(&self, pane: &str, text: &str) -> anyhow::Result<()>;
}

/// The real session this TUI runs in.
pub(crate) struct Zellij {
    session: String,
    repo: PathBuf,
}

impl Zellij {
    /// The session this process is in, else the name `clank open`
    /// would have used; verified to hold the repo's tab.
    pub(crate) fn find(repo: &Path) -> anyhow::Result<Self> {
        open_zellij::require_zellij()?;
        let session = choose_session(None, open_zellij::current_session().as_deref(), repo);
        let panes = open_zellij::snapshot_panes_in(&session).ok_or_else(|| {
            anyhow::anyhow!("zellij did not answer for session `{session}` — is it running?")
        })?;
        if open_zellij::repo_tab_id(&panes, repo).is_none() {
            anyhow::bail!(
                "session `{session}` holds none of {}'s panes — no status pane, no agent pane",
                repo.display()
            );
        }
        Ok(Self {
            session,
            repo: repo.to_path_buf(),
        })
    }
}

impl Panes for Zellij {
    fn session(&self) -> &str {
        &self.session
    }
    fn table(&self) -> Option<Vec<PaneMeta>> {
        let panes = open_zellij::snapshot_panes_in(&self.session)?;
        Some(open_zellij::agent_pane_table(&panes, &self.repo))
    }
    fn subscribe(&self, ids: &[String]) -> Option<SubscribeChild> {
        open_zellij::subscribe_panes(&self.session, ids)
    }
    fn say(&self, pane: &str, text: &str) -> anyhow::Result<()> {
        open_zellij::say_to_pane(&self.session, pane, text)
    }
}

/// A running remote: everything it started, owned here, ended
/// together. The listener and its connections, the pane poll and the
/// `zellij subscribe` child it restarts, the transcript tails, the
/// site builder — one cancel signal, and [`Instance::stop`] joins
/// them all before it returns, so a restart never races a producer of
/// the last instance and no stream outlives the remote that served it
/// (clank-tui-runs-the-remote-in-process). Dropping one without
/// `stop` still cancels, aborts and kills; it just does not wait.
pub(crate) struct Instance {
    pub(crate) url: String,
    cancel: tokio::sync::watch::Sender<bool>,
    tasks: tokio::task::JoinSet<()>,
    /// The blocking work in flight — a pane listing, a say — waited
    /// for at stop however long it takes: aborting the task awaiting
    /// it would leave the subprocess running (codex on 369e74e).
    blocking: Arc<Blocking>,
    grace: std::time::Duration,
    subscription: Arc<Mutex<Option<Subscription>>>,
    tails: Arc<Mutex<Tails>>,
    builder: Option<SiteBuilder>,
    feed: Feed,
    repo: PathBuf,
    home: Option<PathBuf>,
}

/// Blocking operations, tracked so `stop` can wait for every one:
/// the awaiter may be cancelled, the operation cannot be, so the
/// handle is kept here and the awaiter reads a channel instead.
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

impl Instance {
    /// Bind the repo's port, subscribe to its panes, and serve. Fails
    /// before anything is started when the port will not bind or
    /// zellij does not answer — nothing to clean up on that path.
    pub(crate) async fn start(
        repo: PathBuf,
        home: Option<PathBuf>,
        panes: Arc<dyn Panes>,
        poll: std::time::Duration,
        grace: std::time::Duration,
    ) -> anyhow::Result<Self> {
        let table = panes
            .table()
            .ok_or_else(|| anyhow::anyhow!("zellij did not answer for `{}`", panes.session()))?;
        let (listener, port) = listen(&repo).await?;
        let feed = Feed::new(64);
        feed.panes(table.clone());
        let subscription = Arc::new(Mutex::new(subscribe(&*panes, &table, &feed)));
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let mut tasks = tokio::task::JoinSet::new();
        let blocking = Arc::new(Blocking::default());
        let say_panes = panes.clone();
        let sayer: Sayer = Arc::new(move |pane, text| say_panes.say(pane, text));
        tasks.spawn(serve(
            listener,
            feed.clone(),
            sayer,
            panes.session().to_string(),
            repo.clone(),
            site_dir(&repo),
            cancelled.clone(),
            blocking.clone(),
        ));
        tasks.spawn(pane_poll(
            panes,
            feed.clone(),
            subscription.clone(),
            cancelled,
            poll,
            blocking.clone(),
        ));
        let builder = SiteBuilder::start(repo.clone(), home.clone());
        Ok(Self {
            url: format!("http://127.0.0.1:{port}"),
            cancel,
            tasks,
            blocking,
            grace,
            subscription,
            tails: Arc::new(Mutex::new(Tails::default())),
            builder: Some(builder),
            feed,
            repo,
            home,
        })
    }

    /// The TUI rebuilt its snapshot: the page's facts follow, the
    /// transcript tails are retargeted to the roster's bindings, and
    /// the site is rebuilt in the background.
    pub(crate) fn observe(&self, snap: &crate::cli::status::StatusSnapshot) {
        let with = {
            let mut tails = self.tails.lock().unwrap_or_else(|e| e.into_inner());
            tails.reap();
            let Tails {
                handles,
                retired,
                next_owner,
            } = &mut *tails;
            reconcile_tails(
                &self.repo,
                self.home.as_deref(),
                snap,
                &self.feed,
                handles,
                retired,
                next_owner,
            )
        };
        self.feed.status(serde_json::json!({
            "facts": crate::cli::status_tui::web_facts(snap, &with),
            "snapshot": snap.to_json(),
        }));
        if let Some(b) = &self.builder {
            b.request();
        }
    }

    /// End everything and wait for it, in order: the cancel signal;
    /// the tasks, each finishing on its own — the accept loop ends its
    /// connections and forwarders, the poll finishes the listing it is
    /// in — within a grace, aborted only past it; then the subscribe
    /// child and its reader, the tails live and retired, and the
    /// builder, joined off the runtime's threads.
    pub(crate) async fn stop(mut self) {
        let _ = self.cancel.send(true);
        let deadline = tokio::time::Instant::now() + self.grace;
        while !self.tasks.is_empty() {
            match tokio::time::timeout_at(deadline, self.tasks.join_next()).await {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {
                    self.tasks.shutdown().await;
                    break;
                }
            }
        }
        // Whatever blocking work an aborted task was awaiting is still
        // running; this is where it is waited for.
        self.blocking.join_all().await;
        let sub = self
            .subscription
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let tails = self.tails.clone();
        let builder = self.builder.take();
        let joined = tokio::task::spawn_blocking(move || {
            if let Some(sub) = sub {
                sub.end();
            }
            let handles: Vec<TailHandle> = {
                let mut tails = tails.lock().unwrap_or_else(|e| e.into_inner());
                let mut all: Vec<TailHandle> = tails.handles.drain().map(|(_, h)| h).collect();
                all.append(&mut tails.retired);
                all
            };
            for h in handles {
                h.stop_and_join();
            }
            if let Some(b) = builder {
                b.stop();
            }
        })
        .await;
        let _ = joined;
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
        // The child dies with its handle; the reader is not waited for
        // here — `stop` is where waiting happens.
        drop(
            self.subscription
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
        );
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

/// Where to listen: the port the repo remembers, else one the OS
/// gives, remembered from then on so the URL stays what it was
/// (a-repo-remembers-its-port). A remembered port that will not bind
/// is somebody else's now — one TUI per repo, and the remote is the
/// TUI's, so it cannot be ours — and a fresh one takes its place.
async fn listen(repo: &Path) -> anyhow::Result<(tokio::net::TcpListener, u16)> {
    let bind = |port: u16| tokio::net::TcpListener::bind(("127.0.0.1", port));
    if let Some(port) = crate::agent_store::web_port(repo)?
        && let Ok(listener) = bind(port).await
    {
        return Ok((listener, port));
    }
    let listener = bind(0)
        .await
        .map_err(|e| anyhow::anyhow!("cannot listen on 127.0.0.1: {e}"))?;
    let port = listener.local_addr()?.port();
    crate::agent_store::record_web_port(repo, port)?;
    Ok((listener, port))
}

/// A `zellij subscribe` child and the thread reading it: the child
/// is killed on drop, and the reader ends at the EOF that follows.
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
                feed.event(ev);
            }
        }
    });
    Some(Subscription { child, reader })
}

/// A transcript being followed for one agent, for one session, from
/// one file. The stop flag is a courtesy; the token the feed's window
/// records for the tail is the guarantee.
struct TailHandle {
    session: String,
    path: PathBuf,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TailHandle {
    /// Stop the tail and wait for it: within one poll of the file.
    fn stop_and_join(mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for TailHandle {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Which transcript each agent on the roster should be showing: the
/// bound session's file, for harnesses that keep one.
type Wanted = std::collections::HashMap<String, (String, PathBuf)>;

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
) -> std::collections::BTreeSet<String> {
    let mut wanted = Wanted::default();
    if let Some(home) = home {
        for a in &snap.agents {
            let Ok(label) = crate::lifecycle::AgentLabel::parse(&a.label) else {
                continue;
            };
            let Ok(Some(cfg)) = crate::agent_store::load_agent_config(repo, &label) else {
                continue;
            };
            let Some(session) = cfg.session else {
                continue;
            };
            if !transcript::has_adapter(session.tool) {
                continue;
            }
            let recorded = session.transcript.as_deref().map(Path::new);
            if let Some(path) =
                transcript::transcript_path(session.tool, session.id.as_str(), recorded, repo, home)
            {
                wanted.insert(a.label.clone(), (session.id.as_str().to_string(), path));
            }
        }
    }
    let roster = snap.agents.iter().map(|a| a.label.clone()).collect();
    retarget_tails(&wanted, &roster, feed, tails, retired, next_owner)
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
            !wanted
                .get(*label)
                .is_some_and(|(sid, path)| sid == &h.session && path == &h.path)
        })
        .map(|(label, _)| label.clone())
        .collect();
    for label in gone {
        if let Some(h) = tails.remove(&label) {
            h.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            retired.push(h);
        }
        if !wanted.contains_key(&label) {
            *next_owner += 1;
            feed.claim_turns(&label, *next_owner, None);
        }
    }
    for (label, (session, path)) in wanted {
        if tails.contains_key(label) {
            continue;
        }
        *next_owner += 1;
        let owner = *next_owner;
        feed.claim_turns(label, owner, Some(session));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (feed, label2, session2, path2, stop2) = (
            feed.clone(),
            label.clone(),
            session.clone(),
            path.clone(),
            stop.clone(),
        );
        let thread =
            std::thread::spawn(move || run_tail(feed, label2, owner, session2, path2, stop2));
        tails.insert(
            label.clone(),
            TailHandle {
                session: session.clone(),
                path: path.clone(),
                stop,
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
const TAIL_POLL: std::time::Duration = std::time::Duration::from_millis(500);

fn run_tail(
    feed: Feed,
    agent: String,
    owner: u64,
    session: String,
    path: PathBuf,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    let tool = if path.to_string_lossy().contains("/.codex/") {
        clank_core::vocab::Tool::Codex
    } else {
        clank_core::vocab::Tool::Claude
    };
    // The file may not exist yet: a session that has not spoken.
    let (mut tail, lines) = loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match transcript::Tail::open(&path, TAIL_WINDOW) {
            Ok(opened) => break opened,
            Err(_) => std::thread::sleep(std::time::Duration::from_secs(2)),
        }
    };
    let mut generation = 1u32;
    feed.turns_reset(
        &agent,
        owner,
        &session,
        generation,
        window_turns(tool, &lines),
    );
    loop {
        std::thread::sleep(TAIL_POLL);
        if stop.load(Ordering::Relaxed) {
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
            for parsed in transcript::parse(tool, line) {
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
fn window_turns(tool: clank_core::vocab::Tool, lines: &[String]) -> Vec<transcript::Turn> {
    let mut turns: Vec<transcript::Turn> = Vec::new();
    for line in lines {
        for parsed in transcript::parse(tool, line) {
            match parsed {
                transcript::Parsed::Turn(t) => turns.push(t),
                transcript::Parsed::ToolOutput { id, output, images } => {
                    if let Some(t) = turns.iter_mut().find(|t| t.id == id)
                        && let transcript::Body::Tool {
                            output: slot,
                            images: shots,
                            ..
                        } = &mut t.body
                    {
                        *slot = Some(output);
                        *shots = images;
                    }
                }
            }
        }
    }
    turns
}

/// How often the pane set is re-listed. A status change is a hint,
/// not the trigger: the TUI creates panes asynchronously after the
/// change the watcher sees, and a resize changes nothing it watches.
pub(crate) const PANE_POLL: std::time::Duration = std::time::Duration::from_secs(4);

/// Re-list the panes every [`PANE_POLL`]; a change of membership
/// restarts the subscription for the panes there are now. Ends on
/// the cancel signal, its subscription with it (`stop` ends that).
async fn pane_poll(
    panes: Arc<dyn Panes>,
    feed: Feed,
    subscription: Arc<Mutex<Option<Subscription>>>,
    mut cancelled: tokio::sync::watch::Receiver<bool>,
    poll: std::time::Duration,
    blocking: Arc<Blocking>,
) {
    loop {
        tokio::select! {
            _ = cancelled.changed() => return,
            _ = tokio::time::sleep(poll) => {}
        }
        // The listing is waited for, not abandoned: a blocking call
        // cannot be cancelled, so cancel is checked after it.
        let listing = panes.clone();
        let listed = blocking.run(move || listing.table()).await;
        if *cancelled.borrow() {
            return;
        }
        let Some(Some(table)) = listed else {
            continue;
        };
        if feed.panes(table.clone()) == TableChange::Membership {
            let fresh = subscribe(&*panes, &table, &feed);
            let old = std::mem::replace(
                &mut *subscription.lock().unwrap_or_else(|e| e.into_inner()),
                fresh,
            );
            if let Some(old) = old {
                old.end();
            }
        }
    }
}

/// A Server-Sent Events body: frames arrive on a channel and go out
/// as they come. Ends when the sender is dropped.
struct SseBody {
    rx: tokio::sync::mpsc::Receiver<String>,
}

impl Body for SseBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.get_mut()
            .rx
            .poll_recv(cx)
            .map(|next| next.map(|s| Ok(Frame::data(Bytes::from(s)))))
    }
}

type Resp = hyper::Response<http_body_util::combinators::BoxBody<Bytes, std::convert::Infallible>>;

fn text(status: u16, body: impl Into<Bytes>, content_type: &str) -> Resp {
    hyper::Response::builder()
        .status(status)
        .header("content-type", content_type)
        .body(Full::new(body.into()).boxed())
        .expect("static response")
}

/// Serve until the listener fails. Each connection is its own task:
/// an events stream holds its connection open for as long as the
/// browser stays, so serving inline would let one browser block the
/// next.
/// Where `clank html` writes the site this server serves under
/// `/html/`.
fn site_dir(repo: &Path) -> PathBuf {
    repo.join(".clank/html")
}

/// Accept until cancelled; every connection is a task of this loop's
/// own set, aborted and joined when it ends — so no stream outlives
/// the remote that served it.
async fn serve(
    listener: tokio::net::TcpListener,
    feed: Feed,
    sayer: Sayer,
    session: String,
    repo: PathBuf,
    site: PathBuf,
    mut cancelled: tokio::sync::watch::Receiver<bool>,
    blocking: Arc<Blocking>,
) {
    let ctx = Arc::new(Ctx {
        feed,
        sayer,
        blocking,
        page: Arc::from(PAGE.replace(
            "window.CLANK_SESSION || 'clank'",
            &format!("{}", serde_json::json!(session)),
        )),
        pages: Pages { repo, site },
        workers: Mutex::new(tokio::task::JoinSet::new()),
        cancelled: cancelled.clone(),
    });
    loop {
        let accepted = tokio::select! {
            _ = cancelled.changed() => break,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, _)) = accepted else { break };
        let io = hyper_util::rt::TokioIo::new(stream);
        let ctx2 = ctx.clone();
        let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
            let ctx = ctx2.clone();
            async move { Ok::<_, std::convert::Infallible>(route(req, &ctx).await) }
        });
        let mut workers = ctx.workers.lock().unwrap_or_else(|e| e.into_inner());
        let mut cancel = cancelled.clone();
        workers.spawn(async move {
            // A connection ends on the cancel too: gracefully, so a
            // response in flight completes and an idle keep-alive is
            // closed rather than waited on for a request that never
            // comes.
            let conn = hyper::server::conn::http1::Builder::new().serve_connection(io, svc);
            tokio::pin!(conn);
            tokio::select! {
                _ = conn.as_mut() => {}
                _ = cancel.changed() => {
                    conn.as_mut().graceful_shutdown();
                    let _ = conn.await;
                }
            }
        });
        // Finished workers are reaped as they come, so the set holds
        // only the live ones.
        while workers.try_join_next().is_some() {}
    }
    // Every connection and forwarder is a worker: told to end by the
    // cancel they share, and waited for here — this loop is what
    // `stop` joins.
    let mut workers = std::mem::take(&mut *ctx.workers.lock().unwrap_or_else(|e| e.into_inner()));
    while let Some(_done) = workers.join_next().await {}
}

/// What a request is answered from, and where its workers go.
struct Ctx {
    feed: Feed,
    sayer: Sayer,
    blocking: Arc<Blocking>,
    page: Arc<str>,
    pages: Pages,
    /// The accept loop's set: connections, and the SSE forwarders
    /// they start, so none is detached.
    workers: Mutex<tokio::task::JoinSet<()>>,
    cancelled: tokio::sync::watch::Receiver<bool>,
}

/// Where the site's pages come from: the built site, and the repo
/// for a commit page the site never builds — and who is serving.
struct Pages {
    repo: PathBuf,
    site: PathBuf,
}

async fn route(req: hyper::Request<hyper::body::Incoming>, ctx: &Ctx) -> Resp {
    match (req.method().as_str(), req.uri().path()) {
        ("GET", "/") => text(200, ctx.page.to_string(), "text/html; charset=utf-8"),
        ("GET", "/events") => events(ctx),
        ("POST", "/say") => say(req, ctx).await,
        ("GET", path) if path.starts_with("/html/") => {
            site_page(&ctx.pages, &path["/html/".len()..])
        }
        _ => text(404, "not here", "text/plain"),
    }
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

fn site_page(pages: &Pages, rel: &str) -> Resp {
    let Some((ctype, file)) = site_file(rel) else {
        return text(404, "not a page of this site", "text/plain");
    };
    if let Ok(body) = std::fs::read_to_string(pages.site.join(&file)) {
        return text(200, body, ctype);
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
            Some(full) => text(
                200,
                crate::cli::html::render_history_commit_page(&pages.repo, &full),
                ctype,
            ),
            None => text(404, "no such commit in this repo", "text/plain"),
        };
    }
    text(
        404,
        "not built yet: the site is rebuilt after each status refresh; try again shortly",
        "text/plain",
    )
}

/// The handoff: subscribe, then the retained state, then live —
/// the order `Feed::connect` documents. A lagged browser gets the
/// state again.
fn events(ctx: &Ctx) -> Resp {
    let (frames, mut rx) = ctx.feed.connect();
    let (tx, body_rx) = tokio::sync::mpsc::channel::<String>(64);
    let feed = ctx.feed.clone();
    let mut cancelled = ctx.cancelled.clone();
    // The forwarder is a worker of the accept loop's, not a task of
    // its own: it ends on the cancel the loop shares, or when the
    // browser is gone, and the loop joins it (codex on 55db154).
    ctx.workers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .spawn(async move {
            for f in frames {
                if tx.send(f).await.is_err() {
                    return;
                }
            }
            loop {
                let received = tokio::select! {
                    _ = cancelled.changed() => return,
                    r = rx.recv() => r,
                };
                let next = match received {
                    Ok(f) => vec![f],
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => feed.resync(),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                };
                for f in next {
                    if tx.send(f).await.is_err() {
                        return;
                    }
                }
            }
        });
    hyper::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(SseBody { rx: body_rx }.boxed())
        .expect("static response")
}

#[derive(serde::Deserialize)]
struct Say {
    pane: String,
    text: String,
}

async fn say(req: hyper::Request<hyper::body::Incoming>, ctx: &Ctx) -> Resp {
    let body = http_body_util::Limited::new(req.into_body(), 64 * 1024);
    let Ok(bytes) = body.collect().await.map(|c| c.to_bytes()) else {
        return text(413, "too much to say at once", "text/plain");
    };
    let Ok(say) = serde_json::from_slice::<Say>(&bytes) else {
        return text(400, "expected {\"pane\": …, \"text\": …}", "text/plain");
    };
    // Typing into an agent after the switch is off is exactly what
    // off forbids; a say already under way is waited for by `stop`.
    if *ctx.cancelled.borrow() {
        return text(503, "the remote is off", "text/plain");
    }
    let sayer = ctx.sayer.clone();
    let sent = ctx.blocking.run(move || sayer(&say.pane, &say.text)).await;
    match sent {
        Some(Ok(())) => text(204, "", "text/plain"),
        Some(Err(e)) => text(502, format!("{e:#}"), "text/plain"),
        None => text(500, "the sender panicked", "text/plain"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Moving a binding replaces the shown window at once, empty and
    /// scoped to the new session; clearing it (or moving it to a
    /// harness without a transcript) leaves an empty, session-less
    /// window rather than the last conversation; and in both cases
    /// the superseded tail is refused. None of it waits on a file.
    #[test]
    fn retargeting_replaces_the_window_before_any_tail_reads() {
        use std::collections::{BTreeSet, HashMap};
        let feed = Feed::new(16);
        let mut tails = HashMap::new();
        let mut retired = Vec::new();
        let mut next_owner = 0;
        let roster: BTreeSet<String> = ["claude".to_string(), "codex".to_string()].into();
        let dir = tempfile::tempdir().unwrap();
        let file = |n: &str| dir.path().join(n);
        let mut wanted = Wanted::default();
        wanted.insert("claude".into(), ("s1".into(), file("s1.jsonl")));
        let with = retarget_tails(
            &wanted,
            &roster,
            &feed,
            &mut tails,
            &mut retired,
            &mut next_owner,
        );
        assert_eq!(with, ["claude".to_string()].into());
        let owner1 = feed.snapshot().turns["claude"].owner;
        feed.turns_reset("claude", owner1, "s1", 1, vec![say("a")]);
        assert_eq!(feed.snapshot().turns["claude"].turns.len(), 1);

        let (_, mut rx) = feed.connect();
        wanted.insert("claude".into(), ("s2".into(), file("s2.jsonl")));
        retarget_tails(
            &wanted,
            &roster,
            &feed,
            &mut tails,
            &mut retired,
            &mut next_owner,
        );
        assert_eq!(retired.len(), 1, "the s1 tail is kept for its join");
        let owner2 = feed.snapshot().turns["claude"].owner;
        assert_ne!(owner1, owner2);
        let mut page = feed::PageModel::default();
        page.apply(&rx.try_recv().unwrap());
        assert_eq!(page.agents["claude"].session, "s2");
        assert!(page.agents["claude"].turns.is_empty());
        feed.turns_reset("claude", owner1, "s1", 2, vec![say("stale")]);
        assert_eq!(feed.snapshot().turns["claude"].session, "s2");

        wanted.remove("claude");
        let with = retarget_tails(
            &wanted,
            &roster,
            &feed,
            &mut tails,
            &mut retired,
            &mut next_owner,
        );
        assert!(with.is_empty() && tails.is_empty());
        assert_eq!(retired.len(), 2);
        // Retired tails are joined, not dropped: each thread ends
        // within a poll of the file once its stop flag is set.
        let started = std::time::Instant::now();
        for h in retired.drain(..) {
            h.stop_and_join();
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        page.apply(&rx.try_recv().unwrap());
        assert_eq!(page.agents["claude"].session, "");
        feed.turns_reset("claude", owner2, "s2", 1, vec![say("late")]);
        assert!(feed.snapshot().turns["claude"].turns.is_empty());
        // Nothing to retire for an agent that never had a tail.
        assert!(rx.try_recv().is_err());
    }

    fn say(id: &str) -> transcript::Turn {
        transcript::Turn {
            id: id.into(),
            at: None,
            who: transcript::Who::Agent,
            body: transcript::Body::Text { text: id.into() },
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

    /// The port policy, against real listeners: nothing remembered →
    /// one sampled and remembered; remembered and free → that one;
    /// remembered but taken → a fresh one, remembered in its place.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_repo_remembers_its_port() {
        let repo = repo_with_config();
        assert_eq!(crate::agent_store::web_port(repo.path()).unwrap(), None);
        let (listener, port) = listen(repo.path()).await.unwrap();
        assert_eq!(
            crate::agent_store::web_port(repo.path()).unwrap(),
            Some(port)
        );
        drop(listener);
        let (_l, again) = listen(repo.path()).await.unwrap();
        assert_eq!(again, port, "remembered, and free");
        drop(_l);

        let stranger = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let (_l, fresh) = listen(repo.path()).await.unwrap();
        assert_ne!(fresh, port);
        assert_eq!(
            crate::agent_store::web_port(repo.path()).unwrap(),
            Some(fresh)
        );
        drop(stranger);
    }

    /// The site's shapes and nothing else, decided on the name alone:
    /// a plan's name is whatever `PlanKey` accepts, decoded from the
    /// URL; a separator, however encoded, is not a name.
    #[test]
    fn only_the_sites_own_shapes_are_pages() {
        let sha = "a".repeat(40);
        for ok in [
            "index.html",
            "style.css",
            "plan/foo.html",
            "plan/a-plan_2.v1.html",
            "plan/fix%20UI.html",
            "plan/r%C3%A9sum%C3%A9.html",
            "queue/500-foo.html",
            "stash/foo.html",
            &format!("commit/{sha}.html"),
            "commit/abc1234.html",
        ] {
            assert!(site_file(ok).is_some(), "{ok}");
        }
        assert_eq!(
            site_file("plan/fix%20UI%20r%C3%A9sum%C3%A9.html")
                .unwrap()
                .1,
            "plan/fix UI résumé.html",
            "decoded to the name the builder wrote"
        );
        for bad in [
            "",
            "plan/",
            "plan/.html",
            "plan/.hidden.html",
            "plan/foo",
            "plan/foo.html/x",
            "plan/../style.css",
            "plan/..%2Fstyle.css",
            "plan/%2E%2E.html",
            "plan/a%2Fb.html",
            "plan/a%5Cb.html",
            "plan/a%00b.html",
            "plan/%ZZ.html",
            "plan/%C3.html",
            "plan/_.html",
            "commit/abc.html",
            "commit/xyz1234.html",
            "other/foo.html",
            "secret.txt",
            "/etc/passwd",
        ] {
            assert!(site_file(bad).is_none(), "{bad:?}");
        }
        assert_eq!(site_file("style.css").unwrap().0, "text/css; charset=utf-8");
    }

    #[test]
    fn the_session_is_explicit_then_current_then_the_convention() {
        let repo = Path::new("/x/penlock-experiment");
        assert_eq!(
            choose_session(Some("clank-full-app-sim--38b0"), Some("clank-clank"), repo),
            "clank-full-app-sim--38b0"
        );
        assert_eq!(
            choose_session(None, Some("clank-clank"), repo),
            "clank-clank"
        );
        assert_eq!(choose_session(None, None, repo), "clank-penlock-experiment");
    }

    /// The whole server, in-process, with a recorded sayer and a feed
    /// driven by the test: the page is served, the stream carries the
    /// retained state then a live frame, and a message reaches the
    /// sayer with the pane and text it was given.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_page_the_stream_and_a_message_round_trip() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let feed = Feed::new(16);
        feed.panes(vec![PaneMeta {
            id: "terminal_5".into(),
            label: "claude".into(),
            columns: 100,
            rows: 3,
            exited: false,
        }]);
        let said: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = said.clone();
        let sayer: Sayer = Arc::new(move |pane, text| {
            recorder.lock().unwrap().push((pane.into(), text.into()));
            Ok(())
        });
        let site = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        git(&["init", "--quiet", "--initial-branch=main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.path().join("readme"), "before clank\n").unwrap();
        git(&["add", "readme"]);
        git(&["commit", "--quiet", "-m", "history before adoption"]);
        let old_sha = git(&["rev-parse", "HEAD"]);
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let server = tokio::spawn({
            let feed = feed.clone();
            let (repo, site) = (repo.path().to_path_buf(), site.path().to_path_buf());
            async move {
                serve(
                    listener,
                    feed,
                    sayer,
                    "clank-test".to_string(),
                    repo,
                    site,
                    cancelled,
                    Arc::new(Blocking::default()),
                )
                .await
            }
        });
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();

        // The site: a page and the stylesheet it links, served under
        // /html/ with their own types; a name the builder never
        // writes, or a walk, is not a page; a page not built yet says
        // so.
        std::fs::create_dir_all(site.path().join("plan")).unwrap();
        std::fs::write(
            site.path().join("plan/foo.html"),
            "<link rel=\"stylesheet\" href=\"../style.css\"><h1>foo</h1>",
        )
        .unwrap();
        std::fs::write(site.path().join("style.css"), "h1 { color: red }").unwrap();
        std::fs::write(site.path().join("secret.txt"), "no").unwrap();
        let plan = client
            .get(format!("{base}/html/plan/foo.html"))
            .send()
            .await
            .unwrap();
        assert_eq!(plan.status(), 200);
        assert_eq!(
            plan.headers().get("content-type").unwrap(),
            "text/html; charset=utf-8"
        );
        assert!(plan.text().await.unwrap().contains("<h1>foo</h1>"));
        let css = client
            .get(format!("{base}/html/style.css"))
            .send()
            .await
            .unwrap();
        assert_eq!(css.status(), 200);
        assert_eq!(
            css.headers().get("content-type").unwrap(),
            "text/css; charset=utf-8"
        );
        assert_eq!(css.text().await.unwrap(), "h1 { color: red }");
        // The client resolves `..` before sending; a walk that
        // survives resolution is refused by name (the unit test holds
        // the server's own `..` refusal).
        for walk in [
            "/html/secret.txt",
            "/html/plan/../secret.txt",
            "/html/../Cargo.toml",
        ] {
            let r = client.get(format!("{base}{walk}")).send().await.unwrap();
            assert_eq!(r.status(), 404, "{walk}");
        }
        let r = client
            .get(format!("{base}/html/secret.txt"))
            .send()
            .await
            .unwrap();
        assert!(r.text().await.unwrap().contains("not a page"));
        // A stem with a space and an accent, as the builder writes
        // it, reached through the encoded link.
        std::fs::write(site.path().join("plan/fix UI résumé.html"), "<h1>ok</h1>").unwrap();
        let odd = client
            .get(format!("{base}/html/plan/fix%20UI%20r%C3%A9sum%C3%A9.html"))
            .send()
            .await
            .unwrap();
        assert_eq!(odd.status(), 200);
        assert_eq!(odd.text().await.unwrap(), "<h1>ok</h1>");
        let missing = client
            .get(format!("{base}/html/plan/bar.html"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);
        assert!(missing.text().await.unwrap().contains("not built yet"));
        // A commit the site never wrote — history from before
        // adoption — is rendered on request from git, whole sha or
        // short; a sha git does not have is said to be none.
        for rev in [old_sha.as_str(), &old_sha[..7]] {
            let history = client
                .get(format!("{base}/html/commit/{rev}.html"))
                .send()
                .await
                .unwrap();
            assert_eq!(history.status(), 200, "{rev}");
            let body = history.text().await.unwrap();
            assert!(body.contains("history before adoption"), "{rev}: {body}");
            assert!(body.contains("kind-history"), "{rev}");
            assert!(
                body.contains("before clank"),
                "{rev}: the diff is on the page"
            );
        }
        let none = client
            .get(format!("{base}/html/commit/{}.html", "f".repeat(40)))
            .send()
            .await
            .unwrap();
        assert_eq!(none.status(), 404);
        assert!(none.text().await.unwrap().contains("no such commit"));

        assert_eq!(
            client
                .get(format!("{base}/identity"))
                .send()
                .await
                .unwrap()
                .status(),
            404,
            "no identity route: the remote is the TUI's, nobody asks"
        );

        let page = client.get(&base).send().await.unwrap();
        assert_eq!(page.status(), 200);
        let html = page.text().await.unwrap();
        assert!(html.contains("<title>clank</title>"));
        assert!(
            html.contains("\"clank-test\""),
            "the session is baked into the page"
        );

        // The stream: state first (the table), then a live frame.
        let mut resp = client.get(format!("{base}/events")).send().await.unwrap();
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        let mut got = String::new();
        while !got.contains("event: panes\n") {
            got.push_str(std::str::from_utf8(&resp.chunk().await.unwrap().unwrap()).unwrap());
        }
        assert!(got.contains("terminal_5"));
        feed.event(SubscribeEvent {
            event: "pane_update".into(),
            pane_id: Some("terminal_5".into()),
            viewport: Some(vec!["hello".into()]),
            scrollback: None,
            is_initial: false,
        });
        while !got.contains("event: pane\n") {
            got.push_str(std::str::from_utf8(&resp.chunk().await.unwrap().unwrap()).unwrap());
        }
        assert!(
            got.contains("\"screen\":\"\\u001b[Hhello\\u001b[K\\u001b[J\""),
            "the frame carries what the terminal is told, not the raw rows: {got}"
        );

        // A message.
        let r = client
            .post(format!("{base}/say"))
            .json(&serde_json::json!({"pane": "terminal_5", "text": "-n hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204);
        assert_eq!(
            said.lock().unwrap().as_slice(),
            &[("terminal_5".to_string(), "-n hi".to_string())]
        );
        let bad = client
            .post(format!("{base}/say"))
            .body("not json")
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 400);
        assert_eq!(
            client
                .get(format!("{base}/nope"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );

        // Cancel ends the accept loop and its connections; the task
        // returns rather than being aborted.
        let _ = cancel.send(true);
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("the accept loop ends on cancel")
            .unwrap();
    }
}
