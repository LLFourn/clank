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
//! Loopback only, behind a [`door::Door`]: every route but the two
//! doors asks for a session, and the TUI mints the links that open
//! one (the-tui-mints-the-way-in).

pub(crate) mod dns;
pub(crate) mod door;
mod feed;
mod transcript;
pub(crate) mod tunnel;

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;

use crate::cli::codex_chat;
use crate::cli::open_zellij::{self, PaneMeta, SubscribeChild};
use feed::{Feed, SubscribeEvent, TableChange};

const PAGE: &str = include_str!("page.html");

/// The credential decisions both pages make, inlined into each at
/// `DOOR_ANCHOR`. One file so there is one source of truth, inlined
/// rather than served so the sign-in page needs no second request
/// before it can decide whether to sign itself in — and so the
/// sign-in page's copy is trivially on the unauthenticated side of
/// the door rather than a route that has to be kept there.
const DOOR_JS: &str = include_str!("door.js");

/// Where each page takes the decisions. A page that stops containing
/// it stops getting them, which `rendered_pages` is what notices.
const DOOR_ANCHOR: &str = "/*__CLANK_DOOR__*/";

/// The message box's decisions: what a keypress means, how tall the
/// box should be. Session page only — the sign-in page composes
/// nothing.
const COMPOSER_JS: &str = include_str!("composer.js");

const COMPOSER_ANCHOR: &str = "/*__CLANK_COMPOSER__*/";

/// The expression `page_for` replaces to name the session. The page
/// is not a template — this exact substring is the whole mechanism,
/// so a reflow that splits it serves every session a page called
/// `clank` and nothing fails.
const SESSION_ANCHOR: &str = "window.CLANK_SESSION || 'clank'";

/// The page with this session's name compiled into it.
fn page_for(session: &str) -> String {
    PAGE.replace(SESSION_ANCHOR, &serde_json::json!(session).to_string())
        .replace(DOOR_ANCHOR, DOOR_JS)
        .replace(COMPOSER_ANCHOR, COMPOSER_JS)
}

/// Where each page's own script says which policy admits it. Replaced
/// per RESPONSE, because a nonce that repeats admits anything that saw
/// it once.
const NONCE_ANCHOR: &str = "__CLANK_NONCE__";

/// What this origin may run, load and talk to. It keeps the remote's
/// credential and can type into agents, so the answer is: what this
/// binary shipped, and nothing else
/// (the-credential-is-not-a-cdns-to-take).
///
/// Scripts are admitted by NONCE, not by `'unsafe-inline'` — the pages
/// carry their own blocks and nothing else runs, so markup that
/// arrives from anywhere cannot bring a script with it. Styles are
/// inline-allowed because xterm writes stylesheets at run time; that
/// still admits no stylesheet FILE but this origin's. Images take
/// `data:` because the transcript's pictures arrive that way, and
/// `media:` takes `blob:` for the camera's own preview.
fn policy(nonce: &str) -> String {
    format!(
        "default-src 'none'; \
         script-src 'self' 'nonce-{nonce}'; \
         style-src 'self' 'unsafe-inline'; \
         img-src 'self' data:; \
         media-src 'self' blob:; \
         font-src 'self'; \
         connect-src 'self'; \
         form-action 'self'; \
         base-uri 'none'; \
         frame-ancestors 'none'"
    )
}

/// A page, with the nonce its scripts carry and the policy that
/// admits exactly those.
fn guarded_page(html: &str) -> Resp {
    let nonce = door::random_token();
    let mut resp = text(200, html.replace(NONCE_ANCHOR, &nonce), HTML);
    if let Ok(v) = hyper::header::HeaderValue::from_str(&policy(&nonce)) {
        resp.headers_mut().insert("content-security-policy", v);
    }
    resp
}
const LOGIN: &str = include_str!("login.html");

/// The sign-in page with the door decisions compiled into it.
fn login_page() -> String {
    LOGIN.replace(DOOR_ANCHOR, DOOR_JS)
}
/// What the pages load, all of it from this binary: the sign-in
/// page's QR decoder, the terminal the agent panes are drawn with, and
/// the face their names are set in. Vendored, unmodified, each with
/// its licence and digest in `web/vendor/README.md` — a page that
/// keeps a credential runs nobody else's code
/// (the-credential-is-not-a-cdns-to-take).
const JSQR: &str = include_str!("vendor/jsQR.js");
const XTERM_JS: &str = include_str!("vendor/xterm.js");
const XTERM_CSS: &str = include_str!("vendor/xterm.css");
const PLATE_FONT: &[u8] = include_bytes!("vendor/plate.woff2");
#[cfg(test)]
const VENDOR_NOTICE: &str = include_str!("vendor/README.md");
const HTML: &str = "text/html; charset=utf-8";

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
/// Interrupting one pane, for the same reason `Sayer` exists.
type Stopper = Arc<dyn Fn(&str) -> anyhow::Result<()> + Send + Sync>;

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
    /// Interrupt the pane — Escape, the way its own user would.
    fn stop(&self, pane: &str) -> anyhow::Result<()>;
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
        if let Some(panes) = open_zellij::snapshot_panes_in(&self.session) {
            let label = open_zellij::agent_pane_table(&panes, &self.repo)
                .into_iter()
                .find(|entry| entry.id == pane)
                .and_then(|entry| clank_core::ids::AgentLabel::parse(&entry.label).ok());
            if let Some(label) = label
                && matches!(
                    crate::agent_store::resolve_tool(&self.repo, &label),
                    Ok(clank_core::vocab::Tool::Codex)
                )
            {
                if let Some(bound) = crate::agent_store::load_agent_config(&self.repo, &label)
                    .ok()
                    .flatten()
                    .and_then(|cfg| cfg.session)
                    && bound.tool == clank_core::vocab::Tool::Codex
                    && codex_chat::send(&bound.id.to_string(), text)?
                {
                    return Ok(());
                }
                return open_zellij::say_to_codex_pane(&self.session, pane, text);
            }
        }
        open_zellij::say_to_pane(&self.session, pane, text)
    }
    fn stop(&self, pane: &str) -> anyhow::Result<()> {
        let panes = open_zellij::snapshot_panes_in(&self.session).ok_or_else(|| {
            anyhow::anyhow!("zellij did not answer for session `{}`", self.session)
        })?;
        let table = open_zellij::agent_pane_table(&panes, &self.repo);
        let tool = pane_tool(&self.repo, &table, pane)?;
        open_zellij::stop_pane(&self.session, pane, tool)
    }
}

/// The status frame: the TUI's own derivation of what the page
/// shows, and the snapshot beneath it. `with` is the set whose
/// transcripts are being tailed — empty for the seed, which is
/// published before anything is tailed.
pub(crate) fn status_payload(
    _repo: &Path,
    snap: &crate::cli::status::StatusSnapshot,
    with: &std::collections::BTreeSet<String>,
) -> serde_json::Value {
    serde_json::json!({
        "facts": crate::cli::status_tui::web_facts(snap, with),
        "snapshot": snap.to_json(),
    })
}

/// How an instance is to run: the tunnel it comes up behind, if any;
/// how often its panes are re-listed; how long its tasks are given to
/// end; and what the TUI knew when the start was asked for. One thing,
/// because they are answered together at the one place that starts a
/// remote — and eight positional arguments at that call site said
/// less than these four names do (ruthless on 06e16bf).
pub(crate) struct Startup {
    pub(crate) tunnel: Option<tunnel::Start>,
    pub(crate) poll: std::time::Duration,
    pub(crate) grace: std::time::Duration,
    /// Published before the server can answer anyone; see `start`.
    pub(crate) seed: serde_json::Value,
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
    /// The tunnel's URL, once proven to reach this remote.
    pub(crate) public_url: Option<String>,
    tunnel: Option<tunnel::Tunnel>,
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
        door: Arc<door::Door>,
        how: Startup,
    ) -> anyhow::Result<Self> {
        let Startup {
            mut tunnel,
            poll,
            grace,
            seed,
        } = how;
        let table = panes
            .table()
            .ok_or_else(|| anyhow::anyhow!("zellij did not answer for `{}`", panes.session()))?;
        // The public origin is no longer knowable before the start:
        // an allocated hostname does not exist until the tunnel is
        // handed one, and the tunnel cannot come up until this
        // server is answering its probe. So the server starts with
        // no public origin and is TOLD one when the tunnel reports
        // it — once, for the life of the instance.
        let public: Arc<std::sync::RwLock<Option<Seen>>> = Arc::default();
        let sockets: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
        let (listener, port) = listen(&repo).await?;
        let door_for_serve = door.clone();
        let nonce = door::random_token();
        let feed = Feed::new(64);
        feed.panes(table.clone());
        // Published BEFORE the server can answer anyone, so a browser
        // arriving in the moment between the listener accepting and
        // the TUI's next rebuild is TOLD the facts rather than left to
        // infer them from silence — on an idle repo that next rebuild
        // need not come (a-page-that-has-not-heard-is-not-empty).
        feed.status(seed);
        let subscription = Arc::new(Mutex::new(subscribe(&*panes, &table, &feed)));
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let mut tasks = tokio::task::JoinSet::new();
        let blocking = Arc::new(Blocking::default());
        let say_panes = panes.clone();
        let sayer: Sayer = Arc::new(move |pane, text| say_panes.say(pane, text));
        let stop_panes = panes.clone();
        let stopper: Stopper = Arc::new(move |pane| stop_panes.stop(pane));
        tasks.spawn(serve(
            listener,
            feed.clone(),
            sayer,
            stopper,
            panes.session().to_string(),
            repo.clone(),
            site_dir(&repo),
            cancelled.clone(),
            blocking.clone(),
            door_for_serve,
            nonce.clone(),
            public.clone(),
            sockets,
        ));
        tasks.spawn(pane_poll(
            panes,
            feed.clone(),
            subscription.clone(),
            cancelled.clone(),
            poll,
            blocking.clone(),
        ));
        tasks.spawn(door_poll(door, cancelled, blocking.clone()));
        let builder = SiteBuilder::start(repo.clone(), home.clone());
        let mut instance = Self {
            // `localhost`, not `127.0.0.1`: it is the name a
            // browser on this machine is sent to, and the origin a
            // post from that page carries.
            url: format!("http://localhost:{port}"),
            public_url: None,
            tunnel: None,
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
        };
        // The tunnel comes up against the running server — its
        // readiness is this remote's own nonce read back through the
        // public URL — and a tunnel that does not come up takes the
        // remote down with it: a start is up whole or not at all.
        if let Some(start) = tunnel.as_mut() {
            let Some(home) = instance.home.clone() else {
                instance.stop().await;
                anyhow::bail!("a tunnel needs a home for its lease");
            };
            match tunnel::Tunnel::up(start, &home, port, &nonce).await {
                Ok(t) => {
                    // What the tunnel actually came up on decides the
                    // origin a post may carry and the host a `Secure`
                    // cookie is for.
                    if let Some(seen) = seen_as(&t.url) {
                        *public.write().unwrap_or_else(|e| e.into_inner()) = Some(seen);
                    }
                    instance.public_url = Some(t.url.clone());
                    instance.tunnel = Some(t);
                }
                Err(e) => {
                    instance.stop().await;
                    return Err(e);
                }
            }
        }
        Ok(instance)
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
        self.feed.status(status_payload(&self.repo, snap, &with));
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
        // The public URL goes first: nothing reaches a remote that
        // is ending.
        if let Some(t) = self.tunnel.take() {
            t.stop().await;
        }
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
    tool: clank_core::vocab::Tool,
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
type Wanted = std::collections::HashMap<String, (String, clank_core::vocab::Tool, PathBuf)>;

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
                wanted.insert(
                    a.label.clone(),
                    (session.id.as_str().to_string(), session.tool, path),
                );
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
            !wanted.get(*label).is_some_and(|(sid, tool, path)| {
                sid == &h.session && tool == &h.tool && path == &h.path
            })
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
    for (label, (session, tool, path)) in wanted {
        if tails.contains_key(label) {
            continue;
        }
        *next_owner += 1;
        let owner = *next_owner;
        feed.claim_turns(label, owner, Some(session));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (feed, label2, session2, tool2, path2, stop2) = (
            feed.clone(),
            label.clone(),
            session.clone(),
            *tool,
            path.clone(),
            stop.clone(),
        );
        let thread = std::thread::spawn(move || {
            run_tail(feed, label2, owner, session2, tool2, path2, stop2)
        });
        tails.insert(
            label.clone(),
            TailHandle {
                session: session.clone(),
                tool: *tool,
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
    tool: clank_core::vocab::Tool,
    path: PathBuf,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
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

/// How often the door's files are read again for another TUI's
/// revocation: the sessions are the user's, this remote is one of
/// their TUIs, and a stream here must end when a sibling closes it.
pub(crate) const DOOR_POLL: std::time::Duration = std::time::Duration::from_secs(2);

async fn door_poll(
    door: Arc<door::Door>,
    mut cancelled: tokio::sync::watch::Receiver<bool>,
    blocking: Arc<Blocking>,
) {
    loop {
        tokio::select! {
            _ = cancelled.changed() => return,
            _ = tokio::time::sleep(DOOR_POLL) => {}
        }
        let door = door.clone();
        blocking.run(move || door.refresh()).await;
    }
}

/// What ends a socket besides the peer leaving: the remote switched
/// off, the session's term, the session revoked — here or in another
/// TUI.
struct Authority {
    cancelled: tokio::sync::watch::Receiver<bool>,
    revocations: tokio::sync::watch::Receiver<u64>,
    door: Arc<door::Door>,
    admitted: door::Admitted,
}

impl Authority {
    /// Resolves when this socket's authority is over, and not before:
    /// a revocation of somebody else's session is not this one's.
    /// Cancel-safe, so it may sit in a `select!` arm that loses.
    async fn ended(&mut self) {
        loop {
            tokio::select! {
                _ = self.cancelled.changed() => return,
                _ = tokio::time::sleep_until(self.admitted.expires) => return,
                _ = self.revocations.changed() => {
                    if !self.door.is_live(&self.admitted.id_hash) {
                        return;
                    }
                }
            }
        }
    }
}

/// How long the closing handshake gets before the socket is dropped
/// instead: a peer that has stopped reading will never complete it.
const SOCKET_CLOSE: std::time::Duration = std::time::Duration::from_secs(2);

/// Why a pump stopped, which decides whether anything more may be
/// written to the socket.
enum Ended {
    /// The remote stopped, the term ran out, or the session was
    /// revoked. Nothing further is owed to this peer.
    Authority,
    /// The peer left, or the feed did. A goodbye is due.
    Peer,
}

/// ONE task owns the socket: the authority that ends it, the frames
/// going out, and the frames coming in are selected over together.
///
/// Splitting them — an authority governing a producer, a separate
/// task doing the sending — let queued frames reach a peer after its
/// session was revoked, left a blocked send hanging on a peer that
/// had stopped reading, and noticed no peer that simply went away
/// (codex on 7a2a06d). Reading is not optional either: it is how a
/// close is seen, and how the library gets to answer a ping.
///
/// The write is two steps on purpose. `send` is feed-then-flush, and
/// cancelling it once the sink has ACCEPTED the frame but is still
/// flushing would resubmit that frame on the next pass — a pane
/// frame written to the terminal twice is visible corruption. `feed`
/// can only be cancelled before acceptance, and `flush` is
/// idempotent, so each frame is submitted exactly once however often
/// a control frame interrupts (codex on 0c132af).
async fn pump(
    socket: hyper_tungstenite::HyperWebsocketStream,
    frames: Vec<String>,
    mut rx: tokio::sync::broadcast::Receiver<String>,
    feed: Feed,
    mut authority: Authority,
) {
    use futures_util::{SinkExt, StreamExt};
    use hyper_tungstenite::tungstenite::Message;
    let (mut out, mut incoming) = socket.split();
    let mut queue: std::collections::VecDeque<String> = frames.into();
    // Accepted by the sink, not yet flushed to the transport.
    let mut unflushed = false;
    let ended = loop {
        if let Some(next) = queue.front().cloned() {
            tokio::select! {
                biased;
                () = authority.ended() => break Ended::Authority,
                peer = incoming.next() => match peer {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break Ended::Peer,
                    Some(Ok(_)) => {}
                },
                fed = out.feed(Message::text(next)) => {
                    if fed.is_err() {
                        break Ended::Peer;
                    }
                    queue.pop_front();
                    unflushed = true;
                }
            }
        } else if unflushed {
            tokio::select! {
                biased;
                () = authority.ended() => break Ended::Authority,
                peer = incoming.next() => match peer {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break Ended::Peer,
                    Some(Ok(_)) => {}
                },
                flushed = out.flush() => {
                    if flushed.is_err() {
                        break Ended::Peer;
                    }
                    unflushed = false;
                }
            }
        } else {
            tokio::select! {
                biased;
                () = authority.ended() => break Ended::Authority,
                peer = incoming.next() => match peer {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break Ended::Peer,
                    Some(Ok(_)) => {}
                },
                frame = rx.recv() => match frame {
                    Ok(f) => queue.push_back(f),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        queue.extend(feed.resync())
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break Ended::Peer,
                },
            }
        }
    };
    match ended {
        // DROPPED, not closed. A close writes its frame and flushes
        // the transport on the way out, which would hand a revoked
        // peer the very data this is refusing it (codex on 0c132af).
        Ended::Authority => {
            drop(out);
            drop(incoming);
        }
        // The peer is going anyway; a bounded goodbye is courteous
        // and cannot hand anyone anything they should not have.
        Ended::Peer => {
            let _ = tokio::time::timeout(SOCKET_CLOSE, out.close()).await;
        }
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
    stopper: Stopper,
    session: String,
    repo: PathBuf,
    site: PathBuf,
    mut cancelled: tokio::sync::watch::Receiver<bool>,
    blocking: Arc<Blocking>,
    door: Arc<door::Door>,
    nonce: String,
    public: Arc<std::sync::RwLock<Option<Seen>>>,
    sockets: Arc<std::sync::atomic::AtomicUsize>,
) {
    // A post may come from the page this remote serves: the tunnel's
    // origin when there is one, and the loopback origin the browser
    // on this machine uses.
    let port = listener.local_addr().map(|a| a.port()).unwrap_or_default();
    // The loopback origins are known now; the tunnel's is published
    // later, when it has one.
    let origins: Vec<String> = [
        format!("http://localhost:{port}"),
        format!("http://127.0.0.1:{port}"),
    ]
    .iter()
    .filter_map(|u| seen_as(u).map(|s| s.origin))
    .collect();
    let ctx = Arc::new(Ctx {
        feed,
        sayer,
        stopper,
        blocking,
        page: Arc::from(page_for(&session)),
        pages: Pages { repo, site },
        workers: Mutex::new(tokio::task::JoinSet::new()),
        cancelled: cancelled.clone(),
        door,
        origins,
        nonce,
        uploads: std::sync::atomic::AtomicU64::new(0),
        public,
        sockets,
    });
    loop {
        let accepted = tokio::select! {
            _ = cancelled.changed() => break,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, peer)) = accepted else { break };
        let ctx2 = ctx.clone();
        let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
            let ctx = ctx2.clone();
            async move {
                let mut resp = route(req, peer.ip(), &ctx).await;
                alone_on_its_connection(&mut resp);
                Ok::<_, std::convert::Infallible>(resp)
            }
        });
        let mut workers = ctx.workers.lock().unwrap_or_else(|e| e.into_inner());
        let mut cancel = cancelled.clone();
        workers.spawn(async move {
            // The head is read inside the CONNECTION's own task: doing
            // it in the accept loop would hold every other connection
            // behind one peer's first byte. It is cancelled with the
            // connection too — a peer that opens a socket and sends
            // nothing would otherwise sit in this read forever, and
            // the stop that waits for this worker would spend its
            // whole grace on it.
            let stream = tokio::select! {
                _ = cancel.changed() => return,
                read = repaired(stream) => match read {
                    Ok(stream) => stream,
                    Err(_) => return,
                },
            };
            let io = hyper_util::rt::TokioIo::new(stream);
            // A connection ends on the cancel too: gracefully, so a
            // response in flight completes and an idle keep-alive is
            // closed rather than waited on for a request that never
            // comes.
            // `with_upgrades`, because the live channel is a
            // WebSocket now: without it the handshake is answered
            // and the upgrade never completes.
            let conn = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, svc)
                .with_upgrades();
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

/// A URL as a BROWSER reads it: the origin it puts in `Origin` and
/// the authority it puts in `Host`. Both come from one parse, because
/// two hand-rolled readings of the same string disagree with the
/// browser and with each other — `https://host:443` is sent as
/// `https://host`, and a mixed-case host is sent lowercased, so a raw
/// comparison refused the page's own posts (codex on 9c690a2). The
/// parser normalizes exactly as the browser does: it lowercases the
/// host and drops a port that is the scheme's default.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    /// `https://clank.example.com` — what `Origin` carries.
    origin: String,
    /// `clank.example.com` — what `Host` carries.
    authority: String,
    https: bool,
}

fn seen_as(raw: &str) -> Option<Seen> {
    let url = url::Url::parse(raw).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    Some(Seen {
        origin: url.origin().ascii_serialization(),
        // `port()` is None when it is the scheme's default, which is
        // exactly when the browser leaves it out of `Host`.
        authority: match url.port() {
            Some(p) => format!("{host}:{p}"),
            None => host,
        },
        https: url.scheme() == "https",
    })
}

/// What a request is answered from, and where its workers go.
struct Ctx {
    feed: Feed,
    sayer: Sayer,
    stopper: Stopper,
    blocking: Arc<Blocking>,
    page: Arc<str>,
    pages: Pages,
    /// The accept loop's set: connections, and the SSE forwarders
    /// they start, so none is detached.
    workers: Mutex<tokio::task::JoinSet<()>>,
    cancelled: tokio::sync::watch::Receiver<bool>,
    door: Arc<door::Door>,
    /// The origins a browser may post from: the relying party's.
    origins: Vec<String>,
    /// This start's nonce: what the tunnel probe reads back to know
    /// the public URL is this remote and not another.
    nonce: String,
    /// Bumped per attachment, so two in one second cannot collide.
    uploads: std::sync::atomic::AtomicU64,
    /// The URL the tunnel came up on, once it has: the origin a post
    /// may carry beside the loopback ones, and the host that says a
    /// request arrived over TLS. Empty until the tunnel reports.
    public: Arc<std::sync::RwLock<Option<Seen>>>,
    /// How many sockets are live. A socket that ends must decrement
    /// it, which is how "nothing accumulates" is observable at all.
    sockets: Arc<std::sync::atomic::AtomicUsize>,
}

/// A live socket, counted for as long as its pump runs.
struct Live(Arc<std::sync::atomic::AtomicUsize>);

impl Live {
    fn new(count: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Live(count)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Ctx {
    fn public(&self) -> Option<Seen> {
        self.public
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Where the site's pages come from: the built site, and the repo
/// for a commit page the site never builds — and who is serving.
struct Pages {
    repo: PathBuf,
    site: PathBuf,
}

// ---- the handshake a QUIC connector could not ask for ----

/// Close the connection after an ordinary response, so the next
/// request arrives on a fresh one.
///
/// [`repaired`] reads a connection's FIRST head. A connector that
/// pools would send the handshake as the second request on a socket
/// it reused from the preceding `GET`, where nothing would repair it —
/// and the handshake looks poolable to it precisely BECAUSE the two
/// upgrade fields are missing.
///
/// Done per response rather than with `keep_alive(false)` on the
/// builder: that setting makes hyper write `Connection: close` over
/// the 101's own `Connection: upgrade`, so every well-formed client
/// fails the handshake. The native connector only reads the status
/// line, so it would not have noticed, which is exactly how a browser
/// would have been broken by a passing tunnel test.
///
/// The cost is one loopback connect per request on the
/// connector-to-origin hop; the browser's keep-alive is to the edge,
/// not to us.
pub(crate) fn alone_on_its_connection(resp: &mut Resp) {
    if resp.status() == hyper::StatusCode::SWITCHING_PROTOCOLS {
        return;
    }
    resp.headers_mut().insert(
        hyper::header::CONNECTION,
        hyper::header::HeaderValue::from_static("close"),
    );
}

/// A stream that serves `head` first and then the rest of `inner`.
pub(crate) struct Prefixed<S> {
    head: std::io::Cursor<Vec<u8>>,
    inner: S,
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Prefixed<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let at = self.head.position() as usize;
        let left = self.head.get_ref().len() - at;
        if left > 0 {
            let take = left.min(buf.remaining());
            let bytes = self.head.get_ref()[at..at + take].to_vec();
            buf.put_slice(&bytes);
            self.head.set_position((at + take) as u64);
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Prefixed<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// The largest head we will buffer looking for a handshake to repair.
const HEAD_BOUND: usize = 16 * 1024;

/// One past the blank line that ends a request head, if it is here.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Read this connection's first request head, repair it if it is a
/// handshake the connector could not ask for, and hand back a stream
/// that replays every byte read — repaired head first, then whatever
/// arrived behind it, untouched.
pub(crate) async fn repaired(
    mut stream: tokio::net::TcpStream,
) -> std::io::Result<Prefixed<tokio::net::TcpStream>> {
    use tokio::io::AsyncReadExt as _;
    let mut seen = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let boundary = loop {
        if let Some(at) = find_head_end(&seen) {
            break Some(at);
        }
        if seen.len() >= HEAD_BOUND {
            break None;
        }
        match stream.read(&mut chunk).await? {
            0 => break None,
            n => seen.extend_from_slice(&chunk[..n]),
        }
    };
    // A head whose end never arrived is not ours to touch.
    let replay = match boundary.and_then(|at| restored_head(&seen[..at]).map(|h| (at, h))) {
        Some((at, mut head)) => {
            // Whatever was read past the head goes back verbatim.
            head.extend_from_slice(&seen[at..]);
            head
        }
        None => seen,
    };
    Ok(Prefixed {
        head: std::io::Cursor::new(replay),
        inner: stream,
    })
}

/// Put back the two headers a QUIC connector could not carry, in the
/// BYTES, before hyper parses them.
///
/// Cloudflare's edge terminates the websocket handshake itself and
/// reaches the connector over QUIC, where `Connection` and `Upgrade`
/// are hop-by-hop fields and so forbidden; it signals the upgrade out
/// of band instead. A connector is meant to synthesise them again on
/// the way down to HTTP/1.1, and not every one does. What arrives then
/// is a handshake in every respect but those two lines — the key and
/// the version are end-to-end fields and come through untouched — so
/// that is what this keys on.
///
/// On the wire rather than in the router because hyper decides whether
/// a connection is upgradeable while PARSING the head: a header added
/// afterwards answers 101 and then never hands over the socket, which
/// the peer sees as a reset.
///
/// `None` when there is nothing to do, so the common path copies
/// nothing.
fn restored_head(head: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;
    let fields: Vec<(&str, &str)> = lines
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .map(|(name, value)| (name.trim(), value))
        .collect();
    let has = |want: &str| {
        fields
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(want))
    };
    // Already asked for properly, by a connector that rebuilt it.
    if has("upgrade") || !has("sec-websocket-key") || !has("sec-websocket-version") {
        return None;
    }
    let mut out = String::with_capacity(head.len() + 48);
    out.push_str(request_line);
    out.push_str("\r\n");
    for (name, value) in &fields {
        // Drop the `Connection` the connector invented: it wrote
        // `keep-alive` over the field it should have rebuilt.
        if name.eq_ignore_ascii_case("connection") {
            continue;
        }
        out.push_str(name);
        out.push(':');
        out.push_str(value);
        out.push_str("\r\n");
    }
    out.push_str("Connection: Upgrade\r\nUpgrade: websocket\r\n\r\n");
    Some(out.into_bytes())
}

/// The two doors answer anyone; everything else asks the door for a
/// session first, and a browser without one is sent to sign in
/// while a script gets the refusal to act on. A post is also asked
/// where it came from: the page's origin, or nothing.
async fn route(req: hyper::Request<hyper::body::Incoming>, peer: IpAddr, ctx: &Ctx) -> Resp {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    match (method.as_str(), path.as_str()) {
        ("GET", "/login") => return login(&req, peer, ctx),
        // The decoder, on the unauthenticated side because the page
        // that scans has no session yet. Immutable: it is one
        // published build, and the page fetches it only when someone
        // presses `scan`.
        ("GET", "/jsqr.js") => return decoder(&req),
        // The readiness probes: the nonce, and the nonce as the first
        // event of a stream that stays open. Nothing of the page.
        ("GET", "/instance") => {
            return text(
                200,
                serde_json::json!({ "nonce": ctx.nonce }).to_string(),
                "application/json",
            );
        }
        ("GET", "/instance/stream") => return instance_stream(req, ctx),
        // The paste box. Rate-limited per address like the link
        // door, and only from the page that offers it.
        ("POST", "/login") => {
            if !same_origin(&req, ctx) {
                return text(403, "not from this page", "text/plain");
            }
            if !ctx.door.attempt_allowed(peer) {
                return text(429, "too many attempts — wait a minute", "text/plain");
            }
            return paste(req, ctx).await;
        }
        _ => {}
    }
    let cookie = req.headers().get("cookie").and_then(|v| v.to_str().ok());
    let Some(admitted) = ctx.door.admit(cookie) else {
        return match (method.as_str(), path.as_str()) {
            ("GET", p) if p == "/" || p.starts_with("/html/") => redirect("/login"),
            _ => text(401, "no session: sign in at /login", "text/plain"),
        };
    };
    match (method.as_str(), path.as_str()) {
        ("GET", "/") => guarded_page(&ctx.page),
        ("GET", "/whoami") => text(204, "", "text/plain"),
        // The credential, to a browser that is already in. Every door
        // ends at a page with a session, so this is the one place a
        // device is provisioned, whichever door it came through
        // (the-phone-scans-the-token-and-keeps-it).
        // The terminal and the face its names are set in: behind the
        // door, since only a page with a session draws a pane.
        ("GET", "/xterm.js") => {
            static GZ: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();
            served(
                &req,
                XTERM_JS.as_bytes(),
                "text/javascript; charset=utf-8",
                Some(&GZ),
            )
        }
        ("GET", "/xterm.css") => {
            static GZ: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();
            served(
                &req,
                XTERM_CSS.as_bytes(),
                "text/css; charset=utf-8",
                Some(&GZ),
            )
        }
        // A woff2 carries its own compression; gzip over it spends
        // time to add bytes.
        ("GET", "/plate.woff2") => served(&req, PLATE_FONT, "font/woff2", None),
        ("GET", "/token") => match ctx.door.token_for_session(&admitted.id_hash) {
            Ok(Some(token)) => text(
                200,
                serde_json::json!({ "token": token }).to_string(),
                "application/json",
            ),
            // Revoked between admitting and reading: the same answer a
            // request with no session gets.
            Ok(None) => text(401, "no session: sign in at /login", "text/plain"),
            Err(e) => text(500, format!("the store refused: {e:#}"), "text/plain"),
        },
        ("POST", "/forget") if !same_origin(&req, ctx) => text(403, "cross-origin", "text/plain"),
        // Forgetting is signing out: the store the page clears is only
        // half of it, since a live cookie would fetch the token again
        // on the next load.
        ("POST", "/forget") => match ctx.door.revoke_session(&admitted.id_hash) {
            Ok(()) => end_session(secure_transport(&req, ctx), text(204, "", "text/plain")),
            Err(e) => text(500, format!("the store refused: {e:#}"), "text/plain"),
        },
        ("GET", "/events") => events(req, ctx, admitted),
        ("POST", "/say") if !same_origin(&req, ctx) => {
            text(403, "not from this page", "text/plain")
        }
        ("POST", "/say") => say(req, ctx).await,
        ("POST", "/stop") if !same_origin(&req, ctx) => text(403, "cross-origin", "text/plain"),
        ("POST", "/stop") => stop(req, ctx).await,
        ("POST", "/queue") if !same_origin(&req, ctx) => text(403, "cross-origin", "text/plain"),
        ("POST", "/queue") => queue_action(req, ctx).await,
        ("POST", "/upload") if !same_origin(&req, ctx) => text(403, "cross-origin", "text/plain"),
        ("POST", "/upload") => upload(req, ctx).await,
        ("GET", p) if p.starts_with("/html/") => site_page(&ctx.pages, &p["/html/".len()..]),
        _ => text(404, "not here", "text/plain"),
    }
}

/// The vendored decoder, compressed when the caller takes it that
/// way: 251 KiB raw against 56 on the wire, over a phone's connection.
fn decoder(req: &hyper::Request<hyper::body::Incoming>) -> Resp {
    static GZIPPED: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();
    served(
        req,
        JSQR.as_bytes(),
        "text/javascript; charset=utf-8",
        Some(&GZIPPED),
    )
}

/// A vendored asset, served from the binary: its type, an immutable
/// cache because each is one published build, and gzip for the ones
/// that gain by it — a woff2 is already compressed, and compressing it
/// again spends time to add bytes.
fn served(
    req: &hyper::Request<hyper::body::Incoming>,
    body: &'static [u8],
    ctype: &'static str,
    gz: Option<&'static std::sync::OnceLock<Option<Vec<u8>>>>,
) -> Resp {
    let takes_gzip = accepts_gzip(
        req.headers()
            .get("accept-encoding")
            .and_then(|v| v.to_str().ok()),
    );
    let mut builder = hyper::Response::builder()
        .status(200)
        .header("content-type", ctype)
        .header("cache-control", "public, max-age=31536000, immutable")
        // Two answers live at this URL, and a cache that kept one for
        // a year would hand compressed bytes to a caller that said it
        // could not read them (RFC 9110 §12.5.5).
        .header("vary", "accept-encoding");
    // The header goes on only where there are gzip bytes to label:
    // compressing into a `Vec` cannot fail, but a fallback that served
    // the plain script under `content-encoding: gzip` would be a body
    // no browser could read, and that is not a thing to leave
    // possible (ruthless on 4ab4b43).
    let compressed = gz.filter(|_| takes_gzip).and_then(|cell| {
        cell.get_or_init(|| {
            use std::io::Write;
            let mut w = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
            w.write_all(body).ok()?;
            w.finish().ok()
        })
        .clone()
    });
    let body = match compressed {
        Some(gz) => {
            builder = builder.header("content-encoding", "gzip");
            Bytes::from(gz)
        }
        None => Bytes::from_static(body),
    };
    builder
        .body(Full::new(body).boxed())
        .expect("static response")
}

/// Whether the caller takes gzip, by what `Accept-Encoding` says
/// rather than by what it contains: `gzip;q=0` names gzip to REFUSE
/// it, and `*` accepts what is not named. Weight 0 is refusal;
/// anything else, including no weight at all, is acceptance
/// (RFC 9110 §12.5.3).
fn accepts_gzip(header: Option<&str>) -> bool {
    let Some(header) = header else {
        return false;
    };
    let mut wildcard = None;
    for entry in header.split(',') {
        let mut parts = entry.split(';').map(str::trim);
        let coding = parts.next().unwrap_or_default().to_ascii_lowercase();
        if coding != "gzip" && coding != "x-gzip" && coding != "*" {
            continue;
        }
        let refused = parts.any(|p| {
            p.strip_prefix("q=")
                .and_then(|q| q.trim().parse::<f32>().ok())
                .is_some_and(|q| q <= 0.0)
        });
        if coding == "*" {
            wildcard = Some(!refused);
        } else {
            return !refused;
        }
    }
    wildcard.unwrap_or(false)
}

/// The `Origin` this request carries, normalized the same way the
/// allowlist was, so the two are compared as origins rather than as
/// strings.
fn same_origin(req: &hyper::Request<hyper::body::Incoming>, ctx: &Ctx) -> bool {
    req.headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .and_then(seen_as)
        .is_some_and(|o| {
            ctx.origins.contains(&o.origin) || ctx.public().is_some_and(|p| p.origin == o.origin)
        })
}

fn redirect(to: &str) -> Resp {
    hyper::Response::builder()
        .status(303)
        .header("location", to)
        .body(Full::new(Bytes::new()).boxed())
        .expect("static response")
}

/// A response that opens a session: the cookie, `Secure` when the
/// request itself arrived over https — which a tunnel says with the
/// forwarded proto, and plain loopback never does. Following the
/// REQUEST rather than a configured host is what lets an ephemeral
/// hostname work at all (the-way-in-is-a-token).
fn with_session(_req_host: Option<&str>, secure: bool, token: &str, mut resp: Resp) -> Resp {
    if let Ok(v) = hyper::header::HeaderValue::from_str(&door::session_cookie(token, secure)) {
        resp.headers_mut().insert("set-cookie", v);
    }
    resp
}

/// A response that ends a session: the cookie spent, so the browser
/// stops presenting a name the door no longer knows.
fn end_session(secure: bool, mut resp: Resp) -> Resp {
    if let Ok(v) = hyper::header::HeaderValue::from_str(&door::expired_cookie(secure)) {
        resp.headers_mut().insert("set-cookie", v);
    }
    resp
}

/// Whether this request's transport is https, and so whether the
/// session cookie is `Secure`. TWO sources, because neither alone is
/// enough: a tunnel that terminates TLS at its edge says so in the
/// forwarded proto, which is the only signal an ephemeral hostname
/// has; but a connector that merely forwards bytes (ssh, a plain TCP
/// forward behind a TLS endpoint) adds no header at all, and for
/// those the configured public https host is what says the transport
/// is secure. Trusting only the header silently downgraded those
/// (codex on f71e7b9).
fn secure_transport(req: &hyper::Request<hyper::body::Incoming>, ctx: &Ctx) -> bool {
    let forwarded = req
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|p| p.split(',').next().is_some_and(|p| p.trim() == "https"));
    // The `Host` is compared as an authority, not a string: a client
    // that spells the default port still names the same host.
    let arrived_at = || {
        let host = host_of(req)?;
        seen_as(&format!("https://{host}")).map(|s| s.authority)
    };
    forwarded
        || ctx
            .public()
            .filter(|p| p.https)
            .is_some_and(|p| arrived_at().as_deref() == Some(p.authority.as_str()))
}

fn host_of(req: &hyper::Request<hyper::body::Incoming>) -> Option<String> {
    req.headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// `/login`: the paste box — or, with a link the TUI minted,
/// straight in. A spent link is said on the page, not in the URL it
/// came with.
fn login(req: &hyper::Request<hyper::body::Incoming>, peer: IpAddr, ctx: &Ctx) -> Resp {
    let Some(token) = door::query_token(req.uri().query()) else {
        return guarded_page(&login_page());
    };
    if !ctx.door.attempt_allowed(peer) {
        return text(429, "too many attempts — wait a minute", "text/plain");
    }
    if !ctx.door.consume(door::Link::Login, &token) {
        return redirect("/login?why=link");
    }
    match ctx.door.open_session("the TUI's link") {
        Ok(session) => with_session(
            host_of(req).as_deref(),
            secure_transport(req, ctx),
            &session,
            redirect("/"),
        ),
        Err(e) => text(500, format!("the store refused: {e:#}"), "text/plain"),
    }
}

/// A refusal the page can tell apart. 403 alone cannot: a request
/// from another origin is refused with it too, and only a refusal of
/// the CREDENTIAL means the browser should stop keeping it.
fn refused(r: door::Refused) -> Resp {
    match r {
        door::Refused::Token => text(
            403,
            serde_json::json!({ "refused": "token", "say": r.to_string() }).to_string(),
            "application/json",
        ),
        door::Refused::Store(_) => text(500, r.to_string(), "text/plain"),
    }
}

/// The paste box's post: the token, whole, in a JSON body. A
/// correct one opens the same session a minted link would.
async fn paste(req: hyper::Request<hyper::body::Incoming>, ctx: &Ctx) -> Resp {
    let host = host_of(&req);
    let secure = secure_transport(&req, ctx);
    let body = http_body_util::Limited::new(req.into_body(), 8 * 1024);
    let Ok(bytes) = body.collect().await.map(|c| c.to_bytes()) else {
        return text(413, "too long", "text/plain");
    };
    let Ok(pasted) = serde_json::from_slice::<door::Paste>(&bytes) else {
        return text(400, "expected {\"token\": …}", "text/plain");
    };
    match ctx.door.open_session_for_token(pasted.token.trim()) {
        Err(e) => refused(door::Refused::Store(format!("{e:#}"))),
        Ok(None) => refused(door::Refused::Token),
        Ok(Some(session)) => with_session(
            host.as_deref(),
            secure,
            &session,
            text(204, "", "text/plain"),
        ),
    }
}

/// One event carrying the nonce, then open until the remote ends:
/// a tunnel that buffers streams never delivers the event.
/// The readiness socket: the nonce as one frame, then open until the
/// remote ends. It answers WITHOUT a session — it is how the tunnel
/// is proven before anyone can log in — so it carries the nonce and
/// nothing else, ever (codex on 651e48b).
fn instance_stream(req: hyper::Request<hyper::body::Incoming>, ctx: &Ctx) -> Resp {
    if !hyper_tungstenite::is_upgrade_request(&req) {
        return text(
            426,
            "upgrade to a websocket to prove the tunnel",
            "text/plain",
        );
    }
    let mut req = req;
    let (response, socket) = match hyper_tungstenite::upgrade(&mut req, None) {
        Ok(up) => up,
        Err(e) => return text(400, format!("not a websocket: {e}"), "text/plain"),
    };
    let nonce = ctx.nonce.clone();
    let mut cancelled = ctx.cancelled.clone();
    let sockets = ctx.sockets.clone();
    ctx.workers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .spawn(async move {
            use futures_util::{SinkExt, StreamExt};
            use hyper_tungstenite::tungstenite::Message;
            let Ok(socket) = socket.await else { return };
            let _live = Live::new(sockets);
            let (mut out, mut incoming) = socket.split();
            if out.send(Message::text(nonce)).await.is_err() {
                return;
            }
            // Then READ until the prober goes away. A probe that has
            // its answer disconnects, and a socket nobody reads from
            // notices nothing — every finished probe would otherwise
            // leave a worker behind for the life of the TUI (codex on
            // 7a2a06d).
            loop {
                tokio::select! {
                    biased;
                    _ = cancelled.changed() => break,
                    peer = incoming.next() => match peer {
                        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                        Some(Ok(_)) => {}
                    },
                }
            }
            let _ = tokio::time::timeout(SOCKET_CLOSE, out.close()).await;
        });
    response.map(|b| b.map_err(|e| match e {}).boxed())
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
/// The page's live channel. A WebSocket rather than a stream because
/// the accountless tunnel's edge buffers a response BODY until it
/// completes and so delivers a stream never — an upgrade is not a
/// body (measured, the-tunnel-says-its-own-name). The frames are the
/// same the feed publishes, in the same order, retained state first.
fn events(req: hyper::Request<hyper::body::Incoming>, ctx: &Ctx, admitted: door::Admitted) -> Resp {
    if !hyper_tungstenite::is_upgrade_request(&req) {
        return text(
            426,
            "the live channel is a websocket: connect to /events with an upgrade",
            "text/plain",
        );
    }
    // An upgrade is a request like any other: it carries the page's
    // origin or it is not the page's (codex on 651e48b).
    if !same_origin(&req, ctx) {
        return text(403, "not from this page", "text/plain");
    }
    let (frames, rx) = ctx.feed.connect();
    let authority = Authority {
        cancelled: ctx.cancelled.clone(),
        revocations: ctx.door.watch_revocations(),
        door: ctx.door.clone(),
        admitted,
    };
    let mut req = req;
    let (response, socket) = match hyper_tungstenite::upgrade(&mut req, None) {
        Ok(up) => up,
        Err(e) => return text(400, format!("not a websocket: {e}"), "text/plain"),
    };
    let feed = ctx.feed.clone();
    let sockets = ctx.sockets.clone();
    // ONE worker of the accept loop's, so nothing is detached and the
    // loop joins it when it ends (codex on 55db154).
    ctx.workers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .spawn(async move {
            let Ok(socket) = socket.await else { return };
            let _live = Live::new(sockets);
            pump(socket, frames, rx, feed, authority).await;
        });
    response.map(|b| b.map_err(|e| match e {}).boxed())
}

#[derive(serde::Deserialize)]
struct Say {
    pane: String,
    text: String,
}

#[derive(serde::Deserialize)]
struct Stop {
    pane: String,
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

/// Take a file and put it where the agent can read it.
///
/// The bytes cannot reach an agent as bytes — a pane is a keyboard —
/// so an attachment is a FILE and the message names its path. The
/// name is the server's: the client says only what type it is, and a
/// type it invents can at worst produce `.bin`.
async fn upload(req: hyper::Request<hyper::body::Incoming>, ctx: &Ctx) -> Resp {
    let media_type = req
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let body = http_body_util::Limited::new(req.into_body(), 12 * 1024 * 1024);
    let Ok(bytes) = body.collect().await.map(|c| c.to_bytes()) else {
        return text(413, "that file is too big to attach", "text/plain");
    };
    if bytes.is_empty() {
        return text(400, "an empty file", "text/plain");
    }
    if *ctx.cancelled.borrow() {
        return text(503, "the remote is off", "text/plain");
    }
    let dir = ctx.pages.repo.join(".clank/attachments");
    if std::fs::create_dir_all(&dir).is_err() {
        return text(500, "cannot keep attachments here", "text/plain");
    }
    // A name unique to this upload, created with `create_new` so a
    // collision FAILS instead of overwriting. Seconds and the
    // server's nonce are not enough on their own: the nonce is fixed
    // for the whole run, so two files attached in the same second got
    // the same name and the second silently replaced the first —
    // including a path already sent to an agent (codex on a757522).
    let ext = extension_for(&media_type);
    let nonce = &ctx.nonce[..8.min(ctx.nonce.len())];
    let mut path = None;
    for _ in 0..64 {
        let n = ctx
            .uploads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate = dir.join(format!("{}-{nonce}-{n}.{ext}", crate::age::now()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut f) => {
                use std::io::Write as _;
                if f.write_all(&bytes).is_err() {
                    return text(500, "cannot write the attachment", "text/plain");
                }
                path = Some(candidate);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return text(500, "cannot write the attachment", "text/plain"),
        }
    }
    let Some(path) = path else {
        return text(
            500,
            "cannot find a free name for the attachment",
            "text/plain",
        );
    };
    prune_attachments(&dir);
    let Some(shown) = path.to_str() else {
        return text(500, "the attachment has no sayable path", "text/plain");
    };
    text(
        200,
        serde_json::json!({ "path": shown }).to_string(),
        "application/json",
    )
}

/// Interrupt the shown agent with its harness's own control key.
/// Offered whenever an agent might be working; a successful response
/// means the key was delivered, not that a turn is known to have ended.
async fn stop(req: hyper::Request<hyper::body::Incoming>, ctx: &Ctx) -> Resp {
    let body = http_body_util::Limited::new(req.into_body(), 4 * 1024);
    let Ok(bytes) = body.collect().await.map(|c| c.to_bytes()) else {
        return text(413, "too much", "text/plain");
    };
    let Ok(which) = serde_json::from_slice::<Stop>(&bytes) else {
        return text(400, "expected {\"pane\": …}", "text/plain");
    };
    if *ctx.cancelled.borrow() {
        return text(503, "the remote is off", "text/plain");
    }
    let stopper = ctx.stopper.clone();
    match ctx.blocking.run(move || stopper(&which.pane)).await {
        Some(Ok(())) => text(204, "", "text/plain"),
        Some(Err(e)) => text(502, format!("{e:#}"), "text/plain"),
        None => text(500, "the sender panicked", "text/plain"),
    }
}

/// What the page asks of the queue. The handle is a FILE's stem, not
/// a path: what a browser sends names an entry, it does not point at
/// one, and the server resolves it against the queue as it is now
/// (the-queue-is-workable-from-the-phone).
#[derive(serde::Deserialize)]
#[serde(tag = "do", rename_all = "lowercase")]
enum QueueAsk {
    Promote { handle: String },
    Unqueue { handle: String },
    Priority { handle: String, priority: u16 },
}

/// Promote, unqueue or reprioritise one queued plan — the primitives
/// the pane's own page calls, so promoting means one thing on both
/// surfaces. Blocking work (the queue's files, and a promote's commit)
/// runs off the loop, as a say does.
async fn queue_action(req: hyper::Request<hyper::body::Incoming>, ctx: &Ctx) -> Resp {
    let body = http_body_util::Limited::new(req.into_body(), 4 * 1024);
    let Ok(bytes) = body.collect().await.map(|c| c.to_bytes()) else {
        return text(413, "too long", "text/plain");
    };
    let Ok(ask) = serde_json::from_slice::<QueueAsk>(&bytes) else {
        return text(400, "expected {\"do\": …, \"handle\": …}", "text/plain");
    };
    if *ctx.cancelled.borrow() {
        return text(503, "the remote is off", "text/plain");
    }
    let repo = ctx.pages.repo.clone();
    let done = ctx
        .blocking
        .run(move || {
            use crate::cli::queue;
            let handle = match &ask {
                QueueAsk::Promote { handle }
                | QueueAsk::Unqueue { handle }
                | QueueAsk::Priority { handle, .. } => handle.clone(),
            };
            let entry = queue::entry_by_stem(&repo, &handle)?;
            match ask {
                QueueAsk::Promote { .. } => queue::promote_entry(&repo, &entry),
                QueueAsk::Unqueue { .. } => queue::unqueue_entry(&repo, &entry).map(|_| ()),
                QueueAsk::Priority { priority, .. } => {
                    queue::set_priority_of(&repo, &entry, priority).map(|_| ())
                }
            }
        })
        .await;
    match done {
        Some(Ok(())) => text(204, "", "text/plain"),
        // A refusal is the repo's answer — the queue moved, a draft of
        // that name is already there — and the phone is where you are
        // least able to go and look, so it is said rather than dropped.
        Some(Err(e)) => text(
            409,
            serde_json::json!({ "why": format!("{e:#}") }).to_string(),
            "application/json",
        ),
        None => text(500, "the queue's work panicked", "text/plain"),
    }
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
    #[test]
    fn a_stop_resolves_the_tool_of_the_target_pane() {
        use super::*;
        use clank_core::vocab::Tool;
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
            .map(|(i, label)| PaneMeta {
                id: format!("terminal_{i}"),
                label: label.into(),
                columns: 80,
                rows: 24,
                exited: false,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            pane_tool(repo.path(), &table, "terminal_0").unwrap(),
            Tool::Grok
        );
        assert_eq!(
            pane_tool(repo.path(), &table, "terminal_1").unwrap(),
            Tool::Claude
        );
        assert!(pane_tool(repo.path(), &table, "terminal_2").is_err());
    }

    /// The REAL head, captured off a live quick tunnel. Everything
    /// this repair exists for is in these bytes: the key and the
    /// version arrive, the two hop-by-hop fields do not, and the
    /// connector wrote `keep-alive` over the one it should have
    /// rebuilt.
    const CAPTURED: &str = "GET /instance/stream HTTP/1.1\r\n\
         Host: richard-heading-sold-century.trycloudflare.com\r\n\
         X-Forwarded-For: 180.150.5.127\r\n\
         Sec-Websocket-Key: +vKNVs8DZtnNj+E9P07hzA==\r\n\
         Accept-Encoding: gzip\r\n\
         Sec-Websocket-Version: 13\r\n\
         Cf-Visitor: {\"scheme\":\"https\"}\r\n\
         X-Forwarded-Proto: https\r\n\
         Connection: keep-alive\r\n\r\n";

    /// The name on disk is the SERVER's, derived from the media type
    /// the browser declared and nothing else. A client that invents a
    /// type gets `.bin`, which is neither a path nor a program.
    #[test]
    fn a_file_is_named_by_its_type_not_by_its_sender() {
        for (declared, want) in [
            ("image/png", "png"),
            ("image/jpeg", "jpg"),
            ("image/jpeg; charset=binary", "jpg"),
            ("  text/plain  ", "txt"),
            ("application/pdf", "pdf"),
            ("application/octet-stream", "bin"),
            ("", "bin"),
            ("../../etc/passwd", "bin"),
            ("text/html", "bin"),
            ("application/x-sh", "bin"),
        ] {
            assert_eq!(extension_for(declared), want, "{declared:?}");
        }
    }

    /// Attachments are bounded, because nothing else will bound them:
    /// every one is a file on a machine that runs for weeks.
    #[test]
    fn only_the_newest_attachments_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut made = Vec::new();
        for i in 0..ATTACHMENTS_KEPT + 7 {
            let p = dir.path().join(format!("{i:03}.png"));
            std::fs::write(&p, [i as u8]).unwrap();
            // Distinct mtimes, so "newest" means something: a
            // filesystem's resolution is not this test's business.
            let when = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_700_000_000 + i as u64);
            filetime::set_file_mtime(&p, filetime::FileTime::from_system_time(when)).unwrap();
            made.push(p);
        }
        prune_attachments(dir.path());
        let left: std::collections::BTreeSet<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(left.len(), ATTACHMENTS_KEPT, "the cap holds: {left:?}");
        assert!(
            left.contains(&format!("{:03}.png", ATTACHMENTS_KEPT + 6)),
            "the newest is kept: {left:?}"
        );
        assert!(!left.contains("000.png"), "the oldest is not: {left:?}");

        // Under the cap, nothing is touched.
        let few = tempfile::tempdir().unwrap();
        std::fs::write(few.path().join("a.png"), b"a").unwrap();
        prune_attachments(few.path());
        assert!(few.path().join("a.png").exists());
    }

    /// The session's name reaches the page by string replacement, not
    /// by a template: `SESSION_ANCHOR` must survive every edit to
    /// `page.html`. Nothing else notices if it doesn't — the page
    /// still serves, titled `clank`, for every session.
    #[test]
    fn the_session_is_compiled_into_the_page() {
        let page = page_for("a session, \"quoted\"");
        assert!(
            PAGE.contains(SESSION_ANCHOR),
            "the anchor page_for replaces is gone from page.html"
        );
        assert!(
            page.contains(r#""a session, \"quoted\"""#),
            "the session is compiled in, escaped as JSON"
        );
        assert!(
            !page.contains(SESSION_ANCHOR),
            "no un-replaced fallback is left to serve"
        );
    }

    /// Every id the page's script reaches for exists in the markup,
    /// and every id in the markup is reached for. A rename on one
    /// side or a node left behind by a redesign fails here rather
    /// than silently in a browser.
    #[test]
    fn the_page_asks_for_exactly_the_ids_it_has() {
        // The page's own block, wherever its open tag ends: it
        // carries a nonce now, which the policy admits it by.
        let cut = PAGE
            .find("<script nonce")
            .expect("the page has a script of its own");
        let ids: std::collections::BTreeSet<String> = PAGE[..cut]
            .match_indices(" id=\"")
            .map(|(i, m)| {
                let rest = &PAGE[i + m.len()..];
                rest[..rest.find('"').unwrap()].to_string()
            })
            .collect();
        let asked: std::collections::BTreeSet<String> = PAGE[cut..]
            .match_indices("$('")
            .map(|(i, m)| {
                let rest = &PAGE[cut + i + m.len()..];
                rest[..rest.find('\'').unwrap()].to_string()
            })
            .collect();
        assert!(!ids.is_empty() && !asked.is_empty());
        let orphans: Vec<&String> = ids.difference(&asked).collect();
        let missing: Vec<&String> = asked.difference(&ids).collect();
        assert!(
            orphans.is_empty() && missing.is_empty(),
            "in the markup but never asked for: {orphans:?}; asked for but not in the markup: {missing:?}"
        );
    }

    #[test]
    fn the_captured_handshake_is_made_askable() {
        let out = super::restored_head(CAPTURED.as_bytes()).expect("a handshake to repair");
        let out = String::from_utf8(out).unwrap();

        assert!(
            out.starts_with("GET /instance/stream HTTP/1.1\r\n"),
            "{out}"
        );
        assert!(out.contains("\r\nConnection: Upgrade\r\n"), "{out}");
        assert!(out.contains("\r\nUpgrade: websocket\r\n"), "{out}");
        assert!(
            !out.contains("keep-alive"),
            "the invented one is gone: {out}"
        );
        assert!(out.ends_with("\r\n\r\n"), "still a head: {out:?}");

        // Every other field survives, with its value, in order.
        for field in [
            "Host: richard-heading-sold-century.trycloudflare.com",
            "X-Forwarded-For: 180.150.5.127",
            "Sec-Websocket-Key: +vKNVs8DZtnNj+E9P07hzA==",
            "Accept-Encoding: gzip",
            "Sec-Websocket-Version: 13",
            "Cf-Visitor: {\"scheme\":\"https\"}",
            "X-Forwarded-Proto: https",
        ] {
            assert!(out.contains(field), "lost `{field}`: {out}");
        }
        let kept: Vec<&str> = out
            .split("\r\n")
            .filter(|l| l.starts_with("Host:") || l.starts_with("X-Forwarded-For:"))
            .collect();
        assert_eq!(
            kept,
            vec![
                "Host: richard-heading-sold-century.trycloudflare.com",
                "X-Forwarded-For: 180.150.5.127"
            ],
            "order preserved"
        );
    }

    /// Anything that is not a handshake missing its two lines is left
    /// exactly alone — `None`, so the common path copies nothing.
    #[test]
    fn only_the_headerless_handshake_is_touched() {
        let untouched = [
            ("a plain GET", "GET / HTTP/1.1\r\nHost: x\r\n\r\n"),
            (
                "a connector that asked properly",
                "GET /s HTTP/1.1\r\nHost: x\r\nSec-Websocket-Key: k\r\n\
                 Sec-Websocket-Version: 13\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\n\r\n",
            ),
            (
                "a key with no version",
                "GET /s HTTP/1.1\r\nHost: x\r\nSec-Websocket-Key: k\r\n\r\n",
            ),
            (
                "a version with no key",
                "GET /s HTTP/1.1\r\nHost: x\r\nSec-Websocket-Version: 13\r\n\r\n",
            ),
            (
                "a POST",
                "POST /say HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n",
            ),
        ];
        for (what, head) in untouched {
            assert_eq!(super::restored_head(head.as_bytes()), None, "{what}");
        }
    }

    /// The head ends at the blank line, and the boundary is one past
    /// it — so a body is never read, let alone rewritten.
    /// A peer that opens a socket and sends half a request must not
    /// outlive the cancel. The head read is inside the connection's
    /// task, so without selecting it against the cancel the server
    /// waits on this worker for as long as the peer stays silent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_half_sent_request_does_not_outlive_the_cancel() {
        use tokio::io::AsyncWriteExt as _;
        let door = Arc::new(door::Door::new(None));
        let (host, _sockets, cancel, server) = socket_server(door, Feed::new(64)).await;

        // Connected, and deliberately short of the blank line that
        // would end the head.
        let mut half = tokio::net::TcpStream::connect(&host).await.unwrap();
        half.write_all(b"GET /instance/stream HTTP/1.1\r\nHost: x\r\n")
            .await
            .unwrap();
        half.flush().await.unwrap();
        // Let the worker reach the read before cancelling.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let began = std::time::Instant::now();
        cancel.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("the server must not wait on a silent peer")
            .unwrap();
        assert!(
            began.elapsed() < std::time::Duration::from_secs(2),
            "cancel took {:?}, so the read is not under it",
            began.elapsed()
        );
        drop(half);
    }

    #[test]
    fn the_head_ends_at_the_blank_line() {
        let whole = b"POST /say HTTP/1.1\r\nHost: x\r\n\r\n{\"m\":\"a\\r\\n\\r\\nb\"}";
        let at = super::find_head_end(whole).expect("a head end");
        assert_eq!(&whole[..at], b"POST /say HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(super::find_head_end(b"GET / HTTP/1.1\r\nHost: x\r\n"), None);
    }

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
        wanted.insert(
            "claude".into(),
            (
                "s1".into(),
                clank_core::vocab::Tool::Claude,
                file("s1.jsonl"),
            ),
        );
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
        wanted.insert(
            "claude".into(),
            (
                "s2".into(),
                clank_core::vocab::Tool::Claude,
                file("s2.jsonl"),
            ),
        );
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
            body: transcript::Body::prose(id.into()),
        }
    }

    /// A remote behind a connector that forwards bytes and adds no
    /// headers — ssh, a plain TCP forward — still issues a `Secure`
    /// cookie when the request arrived at the configured public
    /// https host. Trusting only the forwarded proto downgraded
    /// exactly these (codex on f71e7b9).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_public_https_host_is_secure_without_a_forwarded_header() {
        // One parse, normalized as the browser normalizes: the host
        // lowercased, a default port dropped from both the origin
        // and the authority.
        let plain = seen_as("https://Clank.Example.com/some/path").unwrap();
        assert_eq!(plain.origin, "https://clank.example.com");
        assert_eq!(plain.authority, "clank.example.com");
        assert!(plain.https);
        let defaulted = seen_as("https://Clank.Example.COM:443").unwrap();
        assert_eq!(
            defaulted, plain,
            "an explicit default port is the same origin"
        );
        let odd = seen_as("https://clank.example.com:8443").unwrap();
        assert_eq!(odd.origin, "https://clank.example.com:8443");
        assert_eq!(odd.authority, "clank.example.com:8443");
        assert!(!seen_as("http://clank.example.com").unwrap().https);
        assert!(seen_as("not a url").is_none());

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        // Spelled with the default port and mixed case, as a config
        // may carry it; the browser will send neither.
        let public = "https://Clank.Example.com:443";
        let door = Arc::new(door::Door::new(None));
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let site = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let server = tokio::spawn({
            let (door, feed) = (door.clone(), Feed::new(8));
            let (repo, site) = (repo.path().to_path_buf(), site.path().to_path_buf());
            let sayer: Sayer = Arc::new(|_, _| Ok(()));
            let stopper: Stopper = Arc::new(|_| Ok(()));
            async move {
                serve(
                    listener,
                    feed,
                    sayer,
                    stopper,
                    "clank-test".to_string(),
                    repo,
                    site,
                    cancelled,
                    Arc::new(Blocking::default()),
                    door,
                    "nonce".to_string(),
                    // Published as the tunnel would publish it.
                    Arc::new(std::sync::RwLock::new(seen_as(public))),
                    Arc::default(),
                )
                .await
            }
        });
        let anon = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let token = door.token().unwrap();
        // The request carries the public Host and NO forwarded proto,
        // as an ssh-style forward would deliver it.
        let r = anon
            .post(format!("http://127.0.0.1:{port}/login"))
            .header("host", "clank.example.com")
            // As a browser sends it: lowercased, no default port.
            .header("origin", "https://clank.example.com")
            .json(&serde_json::json!({ "token": token }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204, "the page's own origin is not a stranger");
        let set = r
            .headers()
            .get("set-cookie")
            .expect("a session")
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            set.contains("; Secure"),
            "the public https host is secure: {set}"
        );

        // The same remote reached on loopback is not https, and its
        // cookie says so — a Secure cookie there would never come back.
        let r = anon
            .post(format!("http://127.0.0.1:{port}/login"))
            .header("origin", format!("http://127.0.0.1:{port}"))
            .json(&serde_json::json!({ "token": token }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204);
        let set = r.headers().get("set-cookie").unwrap().to_str().unwrap();
        assert!(!set.contains("Secure"), "loopback is plain: {set}");

        let _ = cancel.send(true);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
    }

    /// A socket server for the transport's own tests: the feed and
    /// the door are the caller's, so it can revoke, expire and push
    /// frames while a real client is attached.
    async fn socket_server(
        door: Arc<door::Door>,
        feed: Feed,
    ) -> (
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let sockets: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        // Leaked on purpose: the server outlives this function, and
        // a tempdir removed under it would break the site route.
        let site = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let repo = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let server = tokio::spawn({
            let (sockets, door) = (sockets.clone(), door.clone());
            let (repo, site) = (repo.path().to_path_buf(), site.path().to_path_buf());
            let sayer: Sayer = Arc::new(|_, _| Ok(()));
            let stopper: Stopper = Arc::new(|_| Ok(()));
            async move {
                serve(
                    listener,
                    feed,
                    sayer,
                    stopper,
                    "clank-test".to_string(),
                    repo,
                    site,
                    cancelled,
                    Arc::new(Blocking::default()),
                    door,
                    "nonce-of-this-start".to_string(),
                    Arc::default(),
                    sockets,
                )
                .await;
            }
        });
        (format!("localhost:{port}"), sockets, cancel, server)
    }

    fn socket_to(
        host: &str,
        path: &str,
        cookie: Option<&str>,
    ) -> tokio_tungstenite::tungstenite::handshake::client::Request {
        let mut b = tokio_tungstenite::tungstenite::handshake::client::Request::builder()
            .uri(format!("ws://{host}{path}"))
            .header("host", host)
            .header("origin", format!("http://{host}"))
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header(
                "sec-websocket-key",
                tokio_tungstenite::tungstenite::handshake::client::generate_key(),
            );
        if let Some(c) = cookie {
            b = b.header("cookie", c);
        }
        b.body(()).unwrap()
    }

    async fn settles_to_zero(sockets: &std::sync::atomic::AtomicUsize, why: &str) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
        while sockets.load(std::sync::atomic::Ordering::Relaxed) != 0 {
            assert!(tokio::time::Instant::now() < deadline, "{why}");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// The transport itself, not the producer behind it: a peer that
    /// has STOPPED READING fills the socket, and the pump still ends
    /// on revocation rather than staying blocked in a send and
    /// handing the frames over if the peer ever resumes (codex on
    /// 7a2a06d).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_socket_whose_peer_stopped_reading_still_ends_on_revocation() {
        let door = Arc::new(door::Door::new(None));
        let feed = Feed::new(256);
        feed.panes(vec![PaneMeta {
            id: "terminal_5".into(),
            label: "claude".into(),
            columns: 100,
            rows: 3,
            exited: false,
        }]);
        let (host, sockets, _cancel, server) = socket_server(door.clone(), feed.clone()).await;
        let opened = door.open_session("token").unwrap();
        let cookie = format!("{}={opened}", door::COOKIE);

        let (peer, _) =
            tokio_tungstenite::connect_async(socket_to(&host, "/events", Some(&cookie)))
                .await
                .expect("the page's socket");
        // Never read from it again; just keep it alive.
        let peer = Box::new(peer);
        while sockets.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // Push far more than any buffer will hold, so the pump is
        // genuinely blocked in a send with nobody draining. Loopback
        // buffers are generous, so this has to be megabytes, and it
        // has to stay under the feed's capacity or the receiver lags
        // and resyncs to a handful of frames instead.
        let wide = "x".repeat(64 * 1024);
        for _ in 0..200 {
            feed.event(SubscribeEvent {
                event: "pane_update".into(),
                pane_id: Some("terminal_5".into()),
                viewport: Some(vec![wide.clone()]),
                scrollback: None,
                is_initial: false,
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            sockets.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "still attached, and blocked"
        );

        let hash = door.sessions().unwrap()[0].id_hash.clone();
        door.revoke_session(&hash).unwrap();
        settles_to_zero(
            &sockets,
            "a revoked socket ended even though its peer stopped reading",
        )
        .await;
        drop(peer);
        server.abort();
    }

    /// A revoked socket is DROPPED, not closed. A graceful close
    /// writes its frame and flushes the transport on the way out,
    /// which would hand the peer the very data the revocation is
    /// refusing it — so the peer here reads normally, and still gets
    /// no goodbye (codex on 0c132af).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_revoked_socket_is_dropped_without_a_goodbye() {
        use futures_util::StreamExt;
        use hyper_tungstenite::tungstenite::Message;
        let door = Arc::new(door::Door::new(None));
        let feed = Feed::new(64);
        feed.panes(vec![PaneMeta {
            id: "terminal_5".into(),
            label: "claude".into(),
            columns: 100,
            rows: 3,
            exited: false,
        }]);
        let (host, sockets, _cancel, server) = socket_server(door.clone(), feed.clone()).await;
        let opened = door.open_session("token").unwrap();
        let cookie = format!("{}={opened}", door::COOKIE);
        let (mut peer, _) =
            tokio_tungstenite::connect_async(socket_to(&host, "/events", Some(&cookie)))
                .await
                .expect("the page's socket");

        // Read the retained state, so the transport is idle and a
        // close WOULD get through if one were sent.
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), peer.next())
            .await
            .expect("a frame");
        assert!(matches!(first, Some(Ok(Message::Text(_)))));
        while sockets.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let hash = door.sessions().unwrap()[0].id_hash.clone();
        door.revoke_session(&hash).unwrap();
        let saw_close = tokio::time::timeout(std::time::Duration::from_secs(8), async {
            loop {
                match peer.next().await {
                    None | Some(Err(_)) => return false,
                    Some(Ok(Message::Close(_))) => return true,
                    Some(Ok(_)) => {}
                }
            }
        })
        .await
        .expect("the revoked socket ends rather than hanging");
        assert!(
            !saw_close,
            "a revoked socket is dropped, not closed — a close would flush what it held"
        );
        server.abort();
    }

    /// Backpressure interrupted by control frames: the peer pings
    /// while it reads, so the pump's write is cancelled and resumed
    /// again and again. Every frame must arrive EXACTLY once — a
    /// send cancelled after the sink accepted it would resubmit, and
    /// a pane frame written to the terminal twice is corruption
    /// (codex on 0c132af).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_write_interrupted_by_control_frames_delivers_each_frame_once() {
        use futures_util::{SinkExt, StreamExt};
        use hyper_tungstenite::tungstenite::Message;
        let door = Arc::new(door::Door::new(None));
        let feed = Feed::new(512);
        feed.panes(vec![PaneMeta {
            id: "terminal_5".into(),
            label: "claude".into(),
            columns: 100,
            rows: 3,
            exited: false,
        }]);
        let (host, _sockets, _cancel, server) = socket_server(door.clone(), feed.clone()).await;
        let opened = door.open_session("token").unwrap();
        let cookie = format!("{}={opened}", door::COOKIE);
        let (mut peer, _) =
            tokio_tungstenite::connect_async(socket_to(&host, "/events", Some(&cookie)))
                .await
                .expect("the page's socket");

        // Big enough frames that the sink is mid-flush when a
        // control frame lands.
        const SENT: usize = 60;
        let bulk = "y".repeat(48 * 1024);
        for i in 0..SENT {
            feed.event(SubscribeEvent {
                event: "pane_update".into(),
                pane_id: Some("terminal_5".into()),
                viewport: Some(vec![format!("seq-{i}-{bulk}")]),
                scrollback: None,
                is_initial: false,
            });
        }

        let mut seen: Vec<usize> = Vec::new();
        let collected = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while seen.len() < SENT {
                // Ping between reads, so the pump's write keeps
                // losing its select to an incoming control frame.
                let _ = peer.send(Message::Ping(Vec::new().into())).await;
                match peer.next().await {
                    Some(Ok(Message::Text(said))) => {
                        for (i, _) in (0..SENT).map(|i| (i, ())) {
                            if said.contains(&format!("seq-{i}-")) {
                                seen.push(i);
                                break;
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                    None | Some(Err(_)) => return false,
                }
            }
            true
        })
        .await;
        assert!(
            collected.is_ok(),
            "the frames arrived: {} of {SENT}",
            seen.len()
        );

        let mut once = seen.clone();
        once.sort_unstable();
        once.dedup();
        assert_eq!(
            once.len(),
            seen.len(),
            "every frame exactly once, never resubmitted: {seen:?}"
        );
        server.abort();
    }

    /// A peer that simply goes away is noticed, and a probe that has
    /// its answer leaves nothing behind: neither socket may pile up
    /// for the life of the TUI (codex on 7a2a06d).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_peer_that_leaves_is_noticed_and_probes_do_not_pile_up() {
        use futures_util::StreamExt;
        let door = Arc::new(door::Door::new(None));
        let (host, sockets, _cancel, server) = socket_server(door.clone(), Feed::new(16)).await;

        // The readiness socket, connected and dropped ten times.
        for _ in 0..10 {
            let (mut probe, _) =
                tokio_tungstenite::connect_async(socket_to(&host, "/instance/stream", None))
                    .await
                    .expect("no session needed");
            let first = tokio::time::timeout(std::time::Duration::from_secs(5), probe.next())
                .await
                .expect("a frame")
                .unwrap()
                .unwrap();
            assert_eq!(
                first,
                tokio_tungstenite::tungstenite::Message::text("nonce-of-this-start")
            );
            let _ = probe.close(None).await;
            drop(probe);
        }
        settles_to_zero(&sockets, "finished probes left their sockets behind").await;

        // And the live socket: a peer that closes ends the pump.
        let opened = door.open_session("token").unwrap();
        let cookie = format!("{}={opened}", door::COOKIE);
        let (mut peer, _) =
            tokio_tungstenite::connect_async(socket_to(&host, "/events", Some(&cookie)))
                .await
                .expect("the page's socket");
        while sockets.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let _ = peer.close(None).await;
        drop(peer);
        settles_to_zero(&sockets, "an idle peer that closed left its pump running").await;
        server.abort();
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

    /// Establish "remembered, and free" without ever releasing a port
    /// and hoping: ask `listen` for a candidate and let IT tell us
    /// whether the port was free, retrying with the next candidate
    /// when it was not.
    ///
    /// There is no window to lose. A probe that binds a port, drops
    /// it and reports the number is the same assumption this plan
    /// exists to remove — codex ran 16 copies of the earlier attempt
    /// at once and 11 lost that race. Here a lost port is not a
    /// failure, it is the next iteration: `listen` returning
    /// something else means someone held the candidate, which is the
    /// OTHER branch of the policy and equally correct.
    ///
    /// The candidates sit below the OS ephemeral range (49152 and up
    /// here, 32768 on Linux) so the allocator cannot hand one to a
    /// test that asked for any port, and the band is offset by pid so
    /// concurrent copies of this test do not walk in step.
    ///
    /// The teeth: a policy that ignored the remembered port would
    /// return a fresh one every time and exhaust the whole band.
    async fn remembered_and_free(repo: &Path) -> (tokio::net::TcpListener, u16) {
        let stagger = (std::process::id() % 89) as u16 * 8;
        for candidate in (21_037 + stagger)..(21_037 + stagger + 64) {
            crate::agent_store::record_web_port(repo, candidate).unwrap();
            let (listener, got) = listen(repo).await.unwrap();
            if got == candidate {
                return (listener, candidate);
            }
        }
        panic!("64 candidates below the ephemeral range, every one of them taken");
    }

    /// The port policy, against real listeners: nothing remembered →
    /// one sampled and remembered; remembered and free → that one;
    /// remembered but taken → a fresh one, remembered in its place.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_repo_remembers_its_port() {
        let repo = repo_with_config();
        assert_eq!(crate::agent_store::web_port(repo.path()).unwrap(), None);

        // Nothing remembered: one is sampled and written down. This
        // phase asserts nothing about a port staying free, so the
        // ephemeral one it samples is fine.
        let (listener, sampled) = listen(repo.path()).await.unwrap();
        assert_eq!(
            crate::agent_store::web_port(repo.path()).unwrap(),
            Some(sampled)
        );
        drop(listener);

        // Remembered and free: that one.
        let (held, port) = remembered_and_free(repo.path()).await;
        assert_eq!(
            crate::agent_store::web_port(repo.path()).unwrap(),
            Some(port),
            "remembered, and free"
        );

        // Remembered but taken: a fresh one, written down in its
        // place. `held` still owns the port, so nothing is released
        // and there is no race to lose — the earlier version dropped
        // it and re-bound a stranger, which is the same defect one
        // line further down (codex on 657d596).
        let (_fresh, fresh) = listen(repo.path()).await.unwrap();
        assert_ne!(fresh, port);
        assert_eq!(
            crate::agent_store::web_port(repo.path()).unwrap(),
            Some(fresh)
        );
        drop(held);
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
        let halted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let halts = halted.clone();
        let stopper: Stopper = Arc::new(move |pane| {
            halts.lock().unwrap().push(pane.into());
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
        let door = Arc::new(door::Door::new(None));
        let server = tokio::spawn({
            let feed = feed.clone();
            let (repo, site) = (repo.path().to_path_buf(), site.path().to_path_buf());
            let door = door.clone();
            async move {
                serve(
                    listener,
                    feed,
                    sayer,
                    stopper,
                    "clank-test".to_string(),
                    repo,
                    site,
                    cancelled,
                    Arc::new(Blocking::default()),
                    door,
                    "nonce-of-this-start".to_string(),
                    Arc::default(),
                    Arc::default(),
                )
                .await
            }
        });
        let base = format!("http://localhost:{port}");
        let ws = format!("ws://localhost:{port}");
        let anon = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        // The readiness probes answer without a session: the nonce,
        // and the nonce as the first event of a stream that stays
        // open.
        let r = anon.get(format!("{base}/instance")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.json::<serde_json::Value>().await.unwrap()["nonce"],
            "nonce-of-this-start"
        );
        // Plain GET is not the live channel any more, and says so.
        assert_eq!(
            anon.get(format!("{base}/instance/stream"))
                .send()
                .await
                .unwrap()
                .status(),
            426
        );
        // The readiness socket: no session, the nonce, and nothing
        // else — it is how the tunnel is proven before anyone can
        // log in (codex on 651e48b).
        {
            use futures_util::StreamExt;
            let (mut probe, _) = tokio_tungstenite::connect_async(format!("{ws}/instance/stream"))
                .await
                .expect("the readiness socket needs no session");
            let first = tokio::time::timeout(std::time::Duration::from_secs(5), probe.next())
                .await
                .expect("a frame")
                .unwrap()
                .unwrap();
            assert_eq!(
                first,
                tokio_tungstenite::tungstenite::Message::text("nonce-of-this-start")
            );
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(300), probe.next())
                    .await
                    .is_err(),
                "the socket stays open and says nothing else"
            );
        }

        // Nothing but the doors answers without a session: a browser
        // is sent to sign in, a script is refused.
        for (path, status) in [
            ("/", 303),
            ("/html/plan/foo.html", 303),
            ("/html/style.css", 303),
            ("/events", 401),
            ("/whoami", 401),
        ] {
            let r = anon.get(format!("{base}{path}")).send().await.unwrap();
            assert_eq!(r.status(), status, "{path}");
            if status == 303 {
                assert_eq!(r.headers().get("location").unwrap(), "/login", "{path}");
            }
        }
        let r = anon
            .post(format!("{base}/say"))
            .header("origin", &base)
            .json(&serde_json::json!({"pane": "terminal_5", "text": "no"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        assert!(said.lock().unwrap().is_empty(), "nothing was typed");
        let r = anon.get(format!("{base}/login")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.text().await.unwrap().contains("Paste the token"));
        assert_eq!(
            anon.get(format!("{base}/register"))
                .send()
                .await
                .unwrap()
                .status(),
            401,
            "the registration ceremony is gone"
        );
        assert_eq!(
            anon.post(format!("{base}/login"))
                .body("{}")
                .send()
                .await
                .unwrap()
                .status(),
            403,
            "a paste from nowhere"
        );

        // The paste box: the wrong token is refused, the right one
        // opens a session, and the token is the user-level one.
        let token = door.token().unwrap();
        let wrong = anon
            .post(format!("{base}/login"))
            .header("origin", &base)
            .json(&serde_json::json!({"token": "not-it"}))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), 403);
        assert!(wrong.headers().get("set-cookie").is_none());
        let right = anon
            .post(format!("{base}/login"))
            .header("origin", &base)
            .json(&serde_json::json!({"token": token}))
            .send()
            .await
            .unwrap();
        assert_eq!(right.status(), 204);
        let pasted = right
            .headers()
            .get("set-cookie")
            .expect("the paste opens a session")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        assert_eq!(
            anon.get(format!("{base}/whoami"))
                .header("cookie", &pasted)
                .send()
                .await
                .unwrap()
                .status(),
            204,
            "the pasted session is a session"
        );

        // A login link the TUI minted: in, once.
        let link = format!("{base}/login?t={}", door.mint(door::Link::Login));
        let welcome = anon.get(&link).send().await.unwrap();
        assert_eq!(welcome.status(), 303);
        assert_eq!(welcome.headers().get("location").unwrap(), "/");
        let set = welcome
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            set.contains("HttpOnly") && set.contains("SameSite=Strict") && !set.contains("Secure"),
            "{set}"
        );
        let cookie = set.split(';').next().unwrap().to_string();
        let again = anon.get(&link).send().await.unwrap();
        assert_eq!(again.status(), 303);
        assert_eq!(again.headers().get("location").unwrap(), "/login?why=link");
        assert!(again.headers().get("set-cookie").is_none());

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("cookie", cookie.parse().unwrap());
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        assert_eq!(
            client
                .get(format!("{base}/whoami"))
                .send()
                .await
                .unwrap()
                .status(),
            204
        );

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

        // The live channel: a socket, needing a session and the
        // page's own origin; state first (the table), then live.
        assert_eq!(
            client
                .get(format!("{base}/events"))
                .send()
                .await
                .unwrap()
                .status(),
            426,
            "a plain GET is not the live channel"
        );
        use futures_util::StreamExt;
        let socket_request = |cookie: Option<&str>, origin: Option<&str>| {
            let mut b = tokio_tungstenite::tungstenite::handshake::client::Request::builder()
                .uri(format!("{ws}/events"))
                .header("host", format!("localhost:{port}"))
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header(
                    "sec-websocket-key",
                    tokio_tungstenite::tungstenite::handshake::client::generate_key(),
                );
            if let Some(c) = cookie {
                b = b.header("cookie", c);
            }
            if let Some(o) = origin {
                b = b.header("origin", o);
            }
            b.body(()).unwrap()
        };
        assert!(
            tokio_tungstenite::connect_async(socket_request(None, Some(&base)))
                .await
                .is_err(),
            "no session, no live channel"
        );
        assert!(
            tokio_tungstenite::connect_async(socket_request(
                Some(&cookie),
                Some("http://evil.example")
            ))
            .await
            .is_err(),
            "an upgrade from elsewhere is not the page's"
        );
        let (mut live, _) =
            tokio_tungstenite::connect_async(socket_request(Some(&cookie), Some(&base)))
                .await
                .expect("the page's own socket");
        let mut got = String::new();
        while !got.contains("event: panes\n") {
            if let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(said))) =
                tokio::time::timeout(std::time::Duration::from_secs(5), live.next())
                    .await
                    .expect("a frame")
            {
                got.push_str(&said);
            }
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
            if let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(said))) =
                tokio::time::timeout(std::time::Duration::from_secs(5), live.next())
                    .await
                    .expect("a frame")
            {
                got.push_str(&said);
            }
        }
        assert!(
            got.contains("\"screen\":\"\\u001b[Hhello\\u001b[K\\u001b[J\""),
            "the frame carries what the terminal is told, not the raw rows: {got}"
        );

        // A message: from the page's origin, or not at all.
        for wrong in [None, Some("http://evil.example")] {
            let mut r = client
                .post(format!("{base}/say"))
                .json(&serde_json::json!({"pane": "terminal_5", "text": "-n hi"}));
            if let Some(o) = wrong {
                r = r.header("origin", o);
            }
            assert_eq!(r.send().await.unwrap().status(), 403, "{wrong:?}");
        }
        assert!(said.lock().unwrap().is_empty());
        let r = client
            .post(format!("{base}/say"))
            .header("origin", &base)
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
            .header("origin", &base)
            .body("not json")
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 400);

        // An attachment becomes a file the agent can be told to read.
        for wrong in [None, Some("http://evil.example")] {
            let mut r = client
                .post(format!("{base}/upload"))
                .header("content-type", "image/png")
                .body("bytes");
            if let Some(o) = wrong {
                r = r.header("origin", o);
            }
            assert_eq!(r.send().await.unwrap().status(), 403, "{wrong:?}");
        }
        let up = client
            .post(format!("{base}/upload"))
            .header("origin", &base)
            .header("content-type", "image/png")
            .body("PNGBYTES")
            .send()
            .await
            .unwrap();
        assert_eq!(up.status(), 200);
        let where_ = up.json::<serde_json::Value>().await.unwrap()["path"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            where_.ends_with(".png") && where_.contains(".clank/attachments/"),
            "the server names it, under the gitignored directory: {where_}"
        );
        assert_eq!(std::fs::read(&where_).unwrap(), b"PNGBYTES");
        // Empty is refused, because an empty attachment is a mistake
        // with a path that an agent would go and read.
        assert_eq!(
            client
                .post(format!("{base}/upload"))
                .header("origin", &base)
                .header("content-type", "image/png")
                .body("")
                .send()
                .await
                .unwrap()
                .status(),
            400
        );

        // Two attachments in the same second are two files. The name
        // used to be seconds plus the server's nonce, which is fixed
        // for the whole run — so the second upload silently replaced
        // the first, at a path that may already have been sent to an
        // agent (codex on a757522).
        let mut paths = Vec::new();
        for body in ["FIRST", "SECOND", "THIRD"] {
            let r = client
                .post(format!("{base}/upload"))
                .header("origin", &base)
                .header("content-type", "image/png")
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            paths.push(
                r.json::<serde_json::Value>().await.unwrap()["path"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
        }
        let distinct: std::collections::BTreeSet<&String> = paths.iter().collect();
        assert_eq!(distinct.len(), 3, "three uploads, three files: {paths:?}");
        for (path, body) in paths.iter().zip(["FIRST", "SECOND", "THIRD"]) {
            assert_eq!(
                std::fs::read_to_string(path).unwrap(),
                body,
                "each keeps its own bytes"
            );
        }

        // Stop reaches the pane, and is refused cross-origin like
        // anything else that types into an agent.
        for wrong in [None, Some("http://evil.example")] {
            let mut r = client
                .post(format!("{base}/stop"))
                .json(&serde_json::json!({"pane": "terminal_5"}));
            if let Some(o) = wrong {
                r = r.header("origin", o);
            }
            assert_eq!(r.send().await.unwrap().status(), 403, "{wrong:?}");
        }
        assert!(halted.lock().unwrap().is_empty());
        let r = client
            .post(format!("{base}/stop"))
            .header("origin", &base)
            .json(&serde_json::json!({"pane": "terminal_5"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204);
        assert_eq!(
            halted.lock().unwrap().as_slice(),
            &["terminal_5".to_string()]
        );
        assert_eq!(
            client
                .get(format!("{base}/nope"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );

        // Revoked from the TUI: the open stream ends, and the cookie
        // admits nothing more.
        // The stream belongs to the LINK session, not the pasted
        // one: revoke by what opened it, never by position.
        let hash = door
            .sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.how == "the TUI's link")
            .expect("the link's session")
            .id_hash;
        door.revoke_session(&hash).unwrap();
        let ended = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match live.next().await {
                    None | Some(Err(_)) => break,
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                }
            }
        })
        .await;
        assert!(ended.is_ok(), "the socket closes on revocation");
        assert_eq!(
            client
                .get(format!("{base}/whoami"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );

        // A session at the end of its term: the stream ends by its
        // own deadline, with no request made and no page opened.
        let link = format!("{base}/login?t={}", door.mint(door::Link::Login));
        let welcome = anon.get(&link).send().await.unwrap();
        let cookie = welcome
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        // The one just opened by this link: the earlier link session
        // was revoked above, so it is the only one of its kind.
        let hash = door
            .sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.how == "the TUI's link")
            .expect("the fresh link's session")
            .id_hash;
        door.backdate(
            &hash,
            time::Duration::days(door::SESSION_DAYS) - time::Duration::seconds(2),
        )
        .unwrap();
        // Expiry ends a socket as revocation does, and with no
        // request made in between (codex on 651e48b).
        let (mut ending, _) =
            tokio_tungstenite::connect_async(socket_request(Some(&cookie), Some(&base)))
                .await
                .expect("a session still inside its term");
        let ended = tokio::time::timeout(std::time::Duration::from_secs(6), async {
            loop {
                match ending.next().await {
                    None | Some(Err(_)) => break,
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                }
            }
        })
        .await;
        assert!(ended.is_ok(), "the socket closes at its deadline");

        // Guessing at the link door is rate-limited per address.
        let mut refused_at = None;
        for i in 0..=door::ATTEMPTS_PER_MINUTE {
            let r = anon
                .get(format!("{base}/login?t=guess"))
                .send()
                .await
                .unwrap();
            if r.status() == 429 {
                refused_at = Some(i);
                break;
            }
            assert_eq!(r.status(), 303);
        }
        assert!(
            refused_at.is_some(),
            "the guesses are cut off within the limit"
        );
        assert_eq!(
            anon.get(format!("{base}/login?t=guess"))
                .send()
                .await
                .unwrap()
                .status(),
            429
        );
        // The paste box is behind the SAME limiter: past the limit
        // even the right token is refused, so the box cannot be used
        // to guess around the link door's budget.
        assert_eq!(
            anon.post(format!("{base}/login"))
                .header("origin", &base)
                .json(&serde_json::json!({"token": token}))
                .send()
                .await
                .unwrap()
                .status(),
            429,
            "the paste box consults the rate limit"
        );

        // Cancel ends the accept loop and its connections; the task
        // returns rather than being aborted.
        let _ = cancel.send(true);
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("the accept loop ends on cancel")
            .unwrap();
    }
    use crate::cli::js_guard::{Kind, parse_as, script_tags};

    /// The pages AS SERVED, with the door module compiled in.
    ///
    /// The raw constants carry `/*__CLANK_DOOR__*/` and nothing else:
    /// checking those reads a comment where the decisions will be, so
    /// a module that does not parse, or that collides with the page it
    /// lands in, would ship unread. Same rule as the generated site's
    /// guard — read what the renderer COMPOSES.
    fn rendered_pages() -> Vec<(&'static str, String)> {
        let pages = vec![("page.html", page_for("s")), ("login.html", login_page())];
        for (name, html) in &pages {
            assert!(
                !html.contains(DOOR_ANCHOR),
                "{name}: the door anchor survived rendering, so the page ships without the decisions"
            );
            assert!(
                html.contains("function signInVerdict"),
                "{name}: the door module did not land in this page"
            );
        }
        // The composer is the session page's alone.
        for (name, html) in &pages {
            assert!(
                !html.contains(COMPOSER_ANCHOR),
                "{name}: the composer anchor survived rendering"
            );
            assert_eq!(
                html.contains("function composerKey"),
                *name == "page.html",
                "{name}: only the session page composes",
            );
        }
        pages
    }

    /// The paths the pages load, and the vendored file each is served
    /// from. A page may load nothing else: a script from somebody
    /// else's host runs on the origin that keeps the credential, and
    /// whose code it is, is their decision, not ours.
    const SERVED_FROM_VENDOR: &[(&str, &str)] = &[
        ("/jsqr.js", "vendor/jsQR.js"),
        ("/xterm.js", "vendor/xterm.js"),
    ];

    /// The nonce attribute as the pages carry it, before a response
    /// replaces its value.
    const NONCE_ATTR: &str = concat!("nonce=\"", "__CLANK_NONCE__", "\"");

    /// Every script the binary ships, with the page it came from: the
    /// pages' blocks and the vendored decoder, which this binary
    /// serves as surely as it serves them. Upstream's provenance is
    /// checked elsewhere; that it can RUN is checked here.
    ///
    /// Every `<script` is ACCOUNTED FOR, by its open tag rather than
    /// by a literal `<script>` that an attribute slips past: a page
    /// with one plain block and one attributed module would otherwise
    /// pass while the module went unread — a hole of exactly the kind
    /// this plan exists to close (ruthless on 9473655). A shape
    /// nobody decided about fails rather than being skipped.
    fn shipped_scripts() -> Vec<(&'static str, String, Kind)> {
        let mut out = vec![
            ("vendor/jsQR.js", JSQR.to_string(), Kind::Classic),
            ("vendor/xterm.js", XTERM_JS.to_string(), Kind::Classic),
        ];
        for (page, html) in rendered_pages() {
            for tag in script_tags(&html) {
                let (attrs, source) = (tag.attrs, tag.body);
                match () {
                    // A path of this binary's own: it must be an
                    // asset the inventory holds, so a page cannot load
                    // a script that nothing checks. A src to somebody
                    // ELSE's host is refused outright — that is the
                    // arrangement this page was built to leave
                    // (the-credential-is-not-a-cdns-to-take).
                    _ if attrs.contains("src=") => {
                        let path = attrs
                            .split("src=\"")
                            .nth(1)
                            .and_then(|s| s.split('"').next())
                            .unwrap_or_default();
                        let held = SERVED_FROM_VENDOR
                            .iter()
                            .find(|(served, _)| *served == path)
                            .map(|(_, asset)| *asset);
                        match held {
                            Some(asset) => assert!(
                                out.iter().any(|(name, ..)| *name == asset),
                                "{page}: {path} is served from {asset}, which is not inventoried"
                            ),
                            None => panic!(
                                "{page}: this page loads {path}, which this binary does not serve"
                            ),
                        }
                    }
                    // A nonce is how the policy admits the page's own
                    // block; it says nothing about what the block is.
                    _ if attrs.trim_start_matches(NONCE_ATTR).trim().is_empty() => {
                        out.push((page, source.to_string(), Kind::Classic))
                    }
                    _ if attrs.contains("type=\"module\"") => {
                        out.push((page, source.to_string(), Kind::Module))
                    }
                    _ => panic!("{page}: a script tag nobody decided about: <script {attrs}>"),
                }
            }
        }
        out
    }

    /// Every verdict class the ROW can emit has a colour rule, and
    /// none of them is the approving colour.
    ///
    /// The two ends are asserted TOGETHER on purpose. "the markup
    /// carries a class" and "the stylesheet defines a colour" are
    /// both satisfiable while a refusal still renders green, because
    /// nothing makes the class the row writes and the class the rule
    /// names be the same string. This walks the producer's own values.
    #[test]
    fn every_verdict_the_row_emits_has_its_own_colour() {
        use clank_core::vocab::Verdict;
        let classes = [
            Verdict::Continue,
            Verdict::Finished,
            Verdict::RequestChanges,
            Verdict::Unmarked,
        ]
        .map(crate::cli::status_tui::verdict_class);

        // DISTINCT, one per verdict. Every class having a rule is
        // satisfied by two verdicts sharing one class -- which is the
        // bug itself, a refusal wearing the approving mark's colour.
        let unique: std::collections::BTreeSet<_> = classes.iter().collect();
        assert_eq!(
            unique.len(),
            classes.len(),
            "two verdicts share a class, so one of them cannot be right: {classes:?}",
        );

        // A class token is one word: "requested changes" interpolated
        // raw would be two classes, neither of them ruled.
        for c in classes {
            assert!(
                !c.contains(char::is_whitespace) && !c.is_empty(),
                "`{c}` is not a single class token",
            );
            assert!(
                PAGE.contains(&format!(".row.review .glyph.{c} {{")),
                "the row emits `{c}` and the stylesheet never names it",
            );
        }

        // The variable each rule names.
        let var_of = |class: &str| {
            let at = PAGE
                .find(&format!(".row.review .glyph.{class} {{"))
                .expect("the rule is there");
            let rule = &PAGE[at..PAGE[at..].find('}').unwrap() + at];
            rule.split("color:")
                .nth(1)
                .unwrap()
                .trim()
                .trim_end_matches(';')
                .trim_start_matches("var(")
                .trim_end_matches(')')
                .to_string()
        };
        // ...RESOLVED to the colour it holds. Comparing the
        // declarations would only establish that the rules name
        // different variables, and two variables can hold the same
        // colour -- setting `--changes` to `--ok`'s hex then passes
        // every assertion here while each refusal renders green,
        // which is this plan's entire complaint (ruthless 244972c).
        let colour_of = |class: &str| {
            let name = var_of(class);
            let at = PAGE
                .find(&format!("{name}:"))
                .unwrap_or_else(|| panic!("the palette does not define `{name}`"));
            PAGE[at + name.len() + 1..]
                .split(';')
                .next()
                .expect("the declaration ends")
                .trim()
                .to_string()
        };
        let changes = colour_of("v-changes");
        assert_ne!(
            changes,
            colour_of("v-continue"),
            "a refusal must not be the approving COLOUR, whatever it is called",
        );
        assert_ne!(changes, colour_of("v-finished"));
        assert_ne!(changes, colour_of("v-unmarked"));
        // All four distinct: a shared colour anywhere means two
        // verdicts a reader cannot tell apart.
        let shades: std::collections::BTreeSet<_> = classes.iter().map(|c| colour_of(c)).collect();
        assert_eq!(
            shades.len(),
            classes.len(),
            "two verdicts render the same colour: {shades:?}",
        );

        // And the ROW markup actually carries it. Every assertion
        // above is about a class nothing wears unless the review
        // branch interpolates it into the glyph's attribute -- the
        // call site, unproven by testing both ends (test-the-wiring).
        let review_branch = PAGE
            .split("r.kind === 'review' ?")
            .nth(1)
            .and_then(|s| s.split('\n').next())
            .expect("the page builds review rows");
        assert!(
            review_branch.contains("class=\"glyph ${esc(r.verdict_class"),
            "the glyph span must carry the row's verdict class: {review_branch}",
        );

        // And each names a variable the palette defines -- `colour_of`
        // panics otherwise, so resolving all four is the check.
        for class in classes {
            assert!(
                !colour_of(class).is_empty(),
                "`{class}` resolves to nothing",
            );
        }
    }

    /// The glyph column is sized for the widest mark there is.
    ///
    /// What this CAN establish is the arithmetic: the box keeps the
    /// per-character allowance it had when it was built for
    /// one-character marks, applied to the longest mark the log
    /// actually emits. What it CANNOT is that the browser lays it out
    /// that way -- the glyph advance is the font's, not something a
    /// terminal-side assertion reaches (codex 90780fa).
    #[test]
    fn the_glyph_column_keeps_its_allowance_for_the_widest_mark() {
        use clank_core::vocab::Verdict;
        let widest = [
            Verdict::Continue,
            Verdict::Finished,
            Verdict::RequestChanges,
            Verdict::Unmarked,
        ]
        .into_iter()
        .map(|v| crate::cli::log::verdict_mark(v, false).chars().count())
        .max()
        .expect("there are verdicts");
        assert_eq!(widest, 2, "FINISHED's `✓✓` is the long one");

        // The allowance the box was built with, before it had to hold
        // more than one character.
        const PER_CHAR_EM: f32 = 1.5;
        let at = PAGE
            .find(".row .glyph { display: inline-block; width: ")
            .expect("the glyph box is sized");
        let em: f32 = PAGE[at..]
            .split("width: ")
            .nth(1)
            .and_then(|s| s.split("em").next())
            .and_then(|s| s.trim().parse().ok())
            .expect("its width is in em");
        assert!(
            em >= PER_CHAR_EM * widest as f32,
            "{em}em holds {} characters at {PER_CHAR_EM}em each, and the widest mark is {widest}",
            em / PER_CHAR_EM,
        );
    }

    /// The pages are PARSED, not grepped: a duplicate declaration is a
    /// SyntaxError, a SyntaxError in an inline script is the whole
    /// page, and the page ships inside this binary. It went out once —
    /// a second `remember` beside the view preferences' own — and
    /// every test passed, because a dead page still contains its
    /// strings (a-page-that-does-not-parse-is-not-a-page).
    #[test]
    fn the_pages_scripts_parse() {
        let scripts = shipped_scripts();
        // Named, so a script dropping out of the inventory is a
        // failure rather than a test that quietly checks less.
        for wanted in [
            "page.html",
            "login.html",
            "vendor/jsQR.js",
            "vendor/xterm.js",
        ] {
            let found: usize = scripts
                .iter()
                .filter(|(name, body, _)| *name == wanted && !body.trim().is_empty())
                .count();
            assert!(found > 0, "{wanted} is shipped and must be checked");
        }
        for (page, script, kind) in &scripts {
            if let Err(e) = parse_as(script, *kind) {
                panic!("{page} does not parse as {kind:?}: {e}");
            }
        }
    }

    /// The pages and the routes they call ship together: a page that
    /// asks for a path the server does not answer is a phone that
    /// waits for nothing.
    #[test]
    fn the_pages_call_the_doors_the_server_answers() {
        assert!(PAGE.contains("'/token'"), "the page provisions itself");
        assert!(PAGE.contains("'/forget'"), "and can be forgotten");
        assert!(
            LOGIN.contains("clank.token"),
            "the sign-in page reads what was kept"
        );
        assert!(
            LOGIN.contains("'token'"),
            "and keys its discard on the door's mark for a refused credential"
        );
        assert!(
            LOGIN.contains("'/jsqr.js'") && LOGIN.contains("getUserMedia"),
            "the sign-in page scans with the decoder the server serves"
        );
        // A call's row draws what the derivation said, and a turn
        // recorded before there was one falls back to the input it has
        // always had. Pinned here because deleting either fallback
        // breaks nothing else (ruthless on aff7367).
        assert!(
            PAGE.contains("b.title || b.input"),
            "the collapsed row draws the title, or the input it always had"
        );
        assert!(
            PAGE.contains("b.detail || b.input"),
            "and the expansion draws the call, or that same input"
        );
        // The queue's controls and the route they post to, together.
        assert!(PAGE.contains("'/queue'"), "the page asks the queue route");
        assert!(
            PAGE.contains("data-handle") && PAGE.contains("data-do"),
            "and names an entry by its file, with what to do to it"
        );
        // A status frame arrives for anything; the queue is redrawn
        // only when the queue changed, or an edit and a confirmation
        // are taken from under whoever is making them.
        assert!(
            PAGE.contains("if (sig === queueDrawn) return;"),
            "the queue is redrawn only when it changed"
        );
        // The refusal region is the page's own markup, not part of the
        // list it is about: a request refused because another surface
        // took the last entry has an empty list and a reason to show.
        let markup = &PAGE[..PAGE.find("<script nonce").expect("the page's script")];
        assert!(
            markup.contains("id=\"qwhy\""),
            "the refusal region outlives the queue it is about"
        );
        // Both halves of forgetting, in the page that does it: the
        // door is asked, and the `discard` that answer can produce is
        // wired to the store. WHEN it is produced is the state
        // machine's business and is driven in `door_js_tests`; this
        // asserts only that the page can carry it out.
        assert!(PAGE.contains("'/forget'"), "the page asks the door");
        let discard = PAGE
            .split("case 'discard':")
            .nth(1)
            .expect("the page performs a discard");
        assert!(
            discard.starts_with(" keep.clear();"),
            "the store is cleared with the session: {discard:.80}"
        );
    }

    /// `Accept-Encoding` names codings to REFUSE as well as to take,
    /// so containing "gzip" is not accepting it (RFC 9110 §12.5.3).
    #[test]
    fn a_refusal_of_gzip_is_read_as_one() {
        for taken in [
            "gzip",
            "gzip, deflate, br",
            "deflate, gzip;q=0.5",
            "*",
            "br;q=1.0, *;q=0.5",
            "GZIP",
            "x-gzip",
        ] {
            assert!(accepts_gzip(Some(taken)), "{taken}");
        }
        for refused in [
            "gzip;q=0",
            "gzip;q=0, identity",
            "identity",
            "br, deflate",
            "*;q=0",
            "*;q=0, br",
            "",
        ] {
            assert!(!accepts_gzip(Some(refused)), "{refused}");
        }
        assert!(!accepts_gzip(None), "a caller that said nothing");
        // A named coding beats the wildcard, whichever way each goes.
        assert!(accepts_gzip(Some("*;q=0, gzip")));
        assert!(!accepts_gzip(Some("gzip;q=0, *")));
    }

    /// A blob nobody can check is a blob nobody should ship: the
    /// vendored decoder is the published build, byte for byte, and
    /// `web/vendor/README.md` names this digest beside where it came
    /// from. An edit here fails rather than passing quietly.
    #[test]
    fn the_vendored_assets_are_what_the_notice_says() {
        // The registry's own digest, so this is checkable against
        // where each came from rather than against itself.
        for (what, bytes, sha256, licence) in [
            (
                "jsQR.js",
                JSQR.as_bytes(),
                "bc40c8a15196236b2314db0856f72ca0b49980cd5413b8c852a7349f5fee0859",
                "Apache-2.0",
            ),
            (
                "xterm.js",
                XTERM_JS.as_bytes(),
                "4196e242ef1cf4c2adead8d97f4a772a69576076f70b095e004b4abbb049e7bf",
                "MIT",
            ),
            (
                "xterm.css",
                XTERM_CSS.as_bytes(),
                "f7f724aea2bb620a6482bfb8e4bdecfae1152b0c7facef55fbda61f3b6cfedb2",
                "MIT",
            ),
            (
                "plate.woff2",
                PLATE_FONT,
                "723569a183da9c0bb9fb829bd3a3fb3c6e044e0ad405a6feb6fce8a45480fb52",
                "OFL-1.1",
            ),
        ] {
            let digest: String = {
                use sha2::Digest;
                sha2::Sha256::digest(bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect()
            };
            assert_eq!(digest, sha256, "{what} is not the build the notice names");
            assert!(
                VENDOR_NOTICE.contains(sha256) && VENDOR_NOTICE.contains(licence),
                "{what}: the notice names this file and its licence"
            );
        }
    }

    /// The decoder is served to a browser that has no session yet —
    /// the page that scans is the page that signs in — and compressed
    /// when the caller takes it that way.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_decoder_is_served_before_a_session_and_compressed() {
        let (base, _door, anon, cancel, _site, _repo) = door_only().await;
        let plain = anon
            .get(format!("{base}/jsqr.js"))
            .header("accept-encoding", "identity")
            .send()
            .await
            .unwrap();
        assert_eq!(plain.status(), 200, "no session needed");
        assert_eq!(
            plain.headers().get("content-type").unwrap(),
            "text/javascript; charset=utf-8"
        );
        assert!(
            plain
                .headers()
                .get("cache-control")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("immutable"),
            "fetched once per device"
        );
        let plain_headers = plain
            .headers()
            .get("vary")
            .expect("says what it varied on")
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        assert_eq!(plain.text().await.unwrap().len(), JSQR.len());

        // Asked for compressed, it arrives compressed and says so —
        // this client does not decode for us, so these are the bytes
        // that crossed the wire.
        let zipped = anon
            .get(format!("{base}/jsqr.js"))
            .header("accept-encoding", "gzip, deflate")
            .send()
            .await
            .unwrap();
        assert_eq!(zipped.status(), 200);
        assert_eq!(zipped.headers().get("content-encoding").unwrap(), "gzip");
        let zipped_vary = zipped
            .headers()
            .get("vary")
            .map(|v| v.to_str().unwrap().to_ascii_lowercase());
        let wire = zipped.bytes().await.unwrap();
        assert!(
            wire.len() * 3 < JSQR.len(),
            "{} on the wire against {}",
            wire.len(),
            JSQR.len()
        );
        let mut unpacked = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&wire[..]), &mut unpacked)
            .expect("gzip, as labelled");
        assert_eq!(unpacked.len(), JSQR.len(), "the same script, unpacked");

        // Both answers say what they varied on, or a cache holding one
        // for a year hands it to a caller that cannot read it.
        for r in [&plain_headers, zipped_vary.as_ref().unwrap()] {
            assert_eq!(r.as_str(), "accept-encoding");
        }

        // Named as refused: the whole script, uncompressed, though the
        // header contains the word.
        let refused = anon
            .get(format!("{base}/jsqr.js"))
            .header("accept-encoding", "gzip;q=0, identity")
            .send()
            .await
            .unwrap();
        assert!(refused.headers().get("content-encoding").is_none());
        assert_eq!(refused.text().await.unwrap().len(), JSQR.len());
        let _ = cancel.send(true);
    }

    /// The pages name no host but their own. A script, a stylesheet
    /// or a font from somebody else's host runs on the origin that
    /// keeps the credential and can call `/say`, and whose code that
    /// is, is their decision — so the next convenient CDN link has to
    /// meet this test (the-credential-is-not-a-cdns-to-take).
    #[test]
    fn the_pages_reach_out_to_nobody() {
        // As served: a host named by the door module would reach the
        // browser just as surely as one named in the page.
        for (page, html) in rendered_pages() {
            for (at, _) in html.match_indices("//") {
                // Only in a URL: `//` inside the scripts is a comment.
                let before = &html[..at];
                if !before.ends_with("http:") && !before.ends_with("https:") {
                    continue;
                }
                let line = html[..at].lines().count();
                panic!(
                    "{page}:{line} names another host: {}",
                    &html[at..(at + 60).min(html.len())]
                );
            }
            assert!(
                !html.contains("preconnect") && !html.contains("dns-prefetch"),
                "{page}: not even a hint at one"
            );
        }
    }

    /// The terminal and the face are served from this binary, behind
    /// the door — only a page with a session draws a pane — and the
    /// font arrives as it was stored, since a woff2 is compressed
    /// already.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pages_assets_come_from_this_binary() {
        let (base, door, anon, cancel, _site, _repo) = door_only().await;
        for path in ["/xterm.js", "/xterm.css", "/plate.woff2"] {
            assert_eq!(
                anon.get(format!("{base}{path}"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                401,
                "{path} is behind the door"
            );
        }
        let cookie = anon
            .post(format!("{base}/login"))
            .header("origin", &base)
            .json(&serde_json::json!({ "token": door.token().unwrap() }))
            .send()
            .await
            .unwrap()
            .headers()
            .get("set-cookie")
            .expect("a session")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        for (path, ctype, len) in [
            (
                "/xterm.js",
                "text/javascript; charset=utf-8",
                XTERM_JS.len(),
            ),
            ("/xterm.css", "text/css; charset=utf-8", XTERM_CSS.len()),
            ("/plate.woff2", "font/woff2", PLATE_FONT.len()),
        ] {
            let r = anon
                .get(format!("{base}{path}"))
                .header("cookie", &cookie)
                .header("accept-encoding", "gzip")
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200, "{path}");
            assert_eq!(r.headers().get("content-type").unwrap(), ctype, "{path}");
            let zipped = r.headers().get("content-encoding").is_some();
            let bytes = r.bytes().await.unwrap();
            if zipped {
                let mut out = Vec::new();
                std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&bytes[..]), &mut out)
                    .expect("gzip, as labelled");
                assert_eq!(out.len(), len, "{path} unpacks to what is vendored");
            } else {
                assert_eq!(bytes.len(), len, "{path} is served whole");
            }
        }
        // The font is stored compressed, so it is not compressed again.
        let font = anon
            .get(format!("{base}/plate.woff2"))
            .header("cookie", &cookie)
            .header("accept-encoding", "gzip")
            .send()
            .await
            .unwrap();
        assert!(
            font.headers().get("content-encoding").is_none(),
            "a woff2 gains nothing from gzip"
        );
        let _ = cancel.send(true);
    }

    /// The phone's queue actions land in the queue itself, addressed
    /// by file: the entry the page named is the one that moves, a
    /// handle whose file is gone is refused with the reason rather
    /// than resolved to its sibling, and the guards are `/say`'s
    /// (the-queue-is-workable-from-the-phone).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_queue_moves_by_the_handle_the_page_named() {
        let (base, door, anon, cancel, _site, repo) = door_only().await;
        let q = repo.path().join(".clank/queue");
        std::fs::create_dir_all(&q).unwrap();
        std::fs::write(q.join("100-foo.md"), "# foo\n\nthe first\n").unwrap();
        std::fs::write(q.join("900-foo.md"), "# foo\n\nthe second\n").unwrap();
        let cookie = door
            .open_session("token")
            .map(|t| format!("{}={t}", door::COOKIE))
            .unwrap();
        let ask = |body: serde_json::Value, cookie: Option<String>, origin: bool| {
            let mut r = anon.post(format!("{base}/queue")).json(&body);
            if origin {
                r = r.header("origin", &base);
            }
            if let Some(c) = cookie {
                r = r.header("cookie", c);
            }
            async move { r.send().await.unwrap() }
        };

        // The guards first: `/say`'s, on a route that promotes.
        assert_eq!(
            ask(
                serde_json::json!({"do": "promote", "handle": "100-foo"}),
                None,
                true
            )
            .await
            .status(),
            401,
            "no session"
        );
        assert_eq!(
            ask(
                serde_json::json!({"do": "promote", "handle": "100-foo"}),
                Some(cookie.clone()),
                false
            )
            .await
            .status(),
            403,
            "not from this page"
        );

        // A priority moves THAT file, and the other is untouched.
        let r = ask(
            serde_json::json!({"do": "priority", "handle": "900-foo", "priority": 300}),
            Some(cookie.clone()),
            true,
        )
        .await;
        assert_eq!(r.status(), 204);
        assert!(q.join("300-foo.md").exists() && !q.join("900-foo.md").exists());
        assert!(q.join("100-foo.md").exists(), "its sibling stayed put");
        assert!(
            std::fs::read_to_string(q.join("300-foo.md"))
                .unwrap()
                .contains("the second"),
            "and it is the file that was named"
        );

        // The handle it moved under is spent: it is NOT resolved to
        // the sibling that parses to the same name.
        let r = ask(
            serde_json::json!({"do": "unqueue", "handle": "900-foo"}),
            Some(cookie.clone()),
            true,
        )
        .await;
        assert_eq!(r.status(), 409, "a handle whose file is gone");
        let said: serde_json::Value = r.json().await.unwrap();
        assert!(
            said["why"]
                .as_str()
                .unwrap()
                .contains("no longer in the queue"),
            "and it says why: {said}"
        );
        assert!(q.join("100-foo.md").exists(), "nothing else was taken");

        // Unqueue by the handle that does exist: the body lands in the
        // drafts, which is what unqueue means.
        let r = ask(
            serde_json::json!({"do": "unqueue", "handle": "100-foo"}),
            Some(cookie.clone()),
            true,
        )
        .await;
        assert_eq!(r.status(), 204);
        assert!(
            std::fs::read_to_string(repo.path().join(".clank/drafts/foo.md"))
                .unwrap()
                .contains("the first")
        );
        let _ = cancel.send(true);
    }

    /// A promote from the phone is the pane's promote: the body it
    /// named becomes the active plan and its intro is committed, and
    /// the sibling that parses to the same name stays queued — which
    /// is what addressing an entry by its file is for
    /// (the-queue-is-workable-from-the-phone).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_promote_from_the_page_makes_the_plan_and_leaves_its_sibling() {
        let (base, door, anon, cancel, _site, repo) = door_only().await;
        let q = repo.path().join(".clank/queue");
        std::fs::create_dir_all(&q).unwrap();
        std::fs::write(q.join("100-foo.md"), "# foo\n\nthe queued one\n").unwrap();
        std::fs::write(q.join("900-foo.md"), "# foo\n\nits namesake\n").unwrap();
        let cookie = door
            .open_session("token")
            .map(|t| format!("{}={t}", door::COOKIE))
            .unwrap();

        let r = anon
            .post(format!("{base}/queue"))
            .header("origin", &base)
            .header("cookie", &cookie)
            .json(&serde_json::json!({"do": "promote", "handle": "100-foo"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204);

        // It is the active plan, with the body that was named.
        let plan = std::fs::read_to_string(repo.path().join(".clank/plans/foo.md"))
            .expect("the plan the page named");
        assert!(plan.contains("the queued one"), "{plan}");
        assert!(!q.join("100-foo.md").exists(), "and it left the queue");
        // Its namesake is untouched, still queued.
        assert!(
            std::fs::read_to_string(q.join("900-foo.md"))
                .unwrap()
                .contains("its namesake")
        );

        // The intro is committed, as the pane's promote commits it.
        let log = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).trim(),
            "[foo] intro",
            "the promote committed the intro"
        );
        let _ = cancel.send(true);
    }

    /// A zellij with no panes, for a remote started to be asked one
    /// question.
    struct NoPanes;
    impl Panes for NoPanes {
        fn session(&self) -> &str {
            "s"
        }
        fn table(&self) -> Option<Vec<PaneMeta>> {
            Some(Vec::new())
        }
        fn subscribe(&self, _ids: &[String]) -> Option<SubscribeChild> {
            None
        }
        fn say(&self, _pane: &str, _text: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn stop(&self, _pane: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// A dummy Grok pane writes a reply to the same history the web
    /// tail reads. This checks the server's real send route and live
    /// transcript together, without driving a person's Grok session.
    struct GrokPanes {
        history: PathBuf,
        state: Arc<Mutex<GrokPaneState>>,
    }

    #[derive(Default)]
    struct GrokPaneState {
        busy: bool,
        draft: bool,
        queued: Option<String>,
        refuse_stop: bool,
        interrupted: bool,
    }

    impl Panes for GrokPanes {
        fn session(&self) -> &str {
            "grok-test"
        }
        fn table(&self) -> Option<Vec<PaneMeta>> {
            Some(vec![PaneMeta {
                id: "terminal_77".into(),
                label: "grok".into(),
                columns: 80,
                rows: 24,
                exited: false,
            }])
        }
        fn subscribe(&self, _ids: &[String]) -> Option<SubscribeChild> {
            None
        }
        fn say(&self, pane: &str, text: &str) -> anyhow::Result<()> {
            anyhow::ensure!(pane == "terminal_77", "unknown Grok pane");
            anyhow::ensure!(text != "fail", "Grok pane refused the message");
            let mut state = self.state.lock().unwrap();
            if state.busy {
                state.queued = Some(text.to_string());
                return Ok(());
            }
            drop(state);
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&self.history)?;
            writeln!(
                file,
                "{}",
                serde_json::json!({"type":"user","content":[{"type":"text","text":text}]})
            )?;
            writeln!(
                file,
                "{}",
                serde_json::json!({"type":"assistant","content":"reply from dummy Grok"})
            )?;
            Ok(())
        }
        fn stop(&self, pane: &str) -> anyhow::Result<()> {
            anyhow::ensure!(pane == "terminal_77", "unknown Grok pane");
            let mut state = self.state.lock().unwrap();
            anyhow::ensure!(!state.refuse_stop, "Grok interrupt could not be delivered");
            if state.draft {
                state.draft = false;
                return Ok(());
            }
            state.interrupted = true;
            state.busy = false;
            if let Some(queued) = state.queued.take() {
                use std::io::Write;
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&self.history)?;
                writeln!(
                    file,
                    "{}",
                    serde_json::json!({"type":"user","content":[{"type":"text","text":queued}]})
                )?;
                writeln!(
                    file,
                    "{}",
                    serde_json::json!({"type":"assistant","content":"queued reply after interrupt"})
                )?;
            }
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dummy_grok_repo_chats_through_the_web_route_and_stream() {
        use crate::cli::status_tui::fixtures::{agent_row, snap};
        use clank_core::agent_config::{AgentConfig, Session};
        use clank_core::ids::{AgentLabel, SessionId};
        use clank_core::vocab::{AutoMode, Tool};
        use futures_util::StreamExt;

        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"grok":{"tool":"grok","role":"master"}}}"#,
        )
        .unwrap();
        let session = "grok-test-session";
        let history =
            transcript::transcript_path(Tool::Grok, session, None, repo.path(), home.path())
                .unwrap();
        std::fs::create_dir_all(history.parent().unwrap()).unwrap();
        std::fs::write(
            &history,
            r#"{"type":"user","content":[{"type":"text","text":"earlier question"}]}
{"type":"assistant","content":"earlier answer"}
"#,
        )
        .unwrap();
        crate::agent_store::save_agent_config(
            repo.path(),
            &AgentLabel::parse("grok").unwrap(),
            &AgentConfig {
                session: Some(Session {
                    id: SessionId::parse(session).unwrap(),
                    tool: Tool::Grok,
                    updated_at: "2026-09-26T00:00:00Z".into(),
                    transcript: None,
                }),
                ..Default::default()
            },
        )
        .unwrap();
        let mut s = snap(vec![], vec![]);
        s.agents = vec![agent_row(
            "grok",
            crate::cli::teams_config::RosterRole::Master,
            AutoMode::On,
        )];
        let door = Arc::new(door::Door::new(Some(home.path().to_path_buf())));
        let state = Arc::new(Mutex::new(GrokPaneState::default()));
        let instance = Instance::start(
            repo.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            Arc::new(GrokPanes {
                history: history.clone(),
                state: state.clone(),
            }),
            door.clone(),
            Startup {
                tunnel: None,
                poll: std::time::Duration::from_secs(3600),
                grace: std::time::Duration::from_millis(200),
                seed: status_payload(repo.path(), &s, &Default::default()),
            },
        )
        .await
        .unwrap();
        instance.observe(&s);
        let opened = door.open_session("token").unwrap();
        let cookie = format!("{}={opened}", door::COOKIE);
        let host = instance.url.trim_start_matches("http://").to_string();
        let (mut peer, _) =
            tokio_tungstenite::connect_async(socket_to(&host, "/events", Some(&cookie)))
                .await
                .unwrap();
        let mut frames = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(6);
        while !frames.contains("earlier answer") {
            let frame = tokio::time::timeout_at(deadline, peer.next())
                .await
                .expect("Grok transcript frame")
                .unwrap()
                .unwrap();
            frames.push_str(frame.to_text().unwrap());
        }
        assert!(
            frames.contains("\"transcript\":true"),
            "Grok opens in chat mode: {frames}"
        );
        assert!(
            frames.contains("terminal_77"),
            "the terminal view remains available: {frames}"
        );

        let client = reqwest::Client::new();
        let sent = client
            .post(format!("{}/say", instance.url))
            .header("origin", &instance.url)
            .header("cookie", &cookie)
            .json(&serde_json::json!({"pane":"terminal_77","text":"new question"}))
            .send()
            .await
            .unwrap();
        assert_eq!(sent.status(), 204);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(6);
        while !frames.contains("reply from dummy Grok") {
            let frame = tokio::time::timeout_at(deadline, peer.next())
                .await
                .expect("Grok reply frame")
                .unwrap()
                .unwrap();
            frames.push_str(frame.to_text().unwrap());
        }
        assert!(
            frames.contains("new question"),
            "the submitted prompt appears: {frames}"
        );

        let failed = client
            .post(format!("{}/say", instance.url))
            .header("origin", &instance.url)
            .header("cookie", &cookie)
            .json(&serde_json::json!({"pane":"terminal_77","text":"fail"}))
            .send()
            .await
            .unwrap();
        assert_eq!(failed.status(), 502);
        assert!(
            failed
                .text()
                .await
                .unwrap()
                .contains("Grok pane refused the message")
        );

        // Grok's fixture queues a prompt while busy. A refused Stop
        // leaves it queued; one delivered Ctrl+C clears a local draft,
        // and a second reaches the running turn.
        state.lock().unwrap().busy = true;
        let queued = client
            .post(format!("{}/say", instance.url))
            .header("origin", &instance.url)
            .header("cookie", &cookie)
            .json(&serde_json::json!({"pane":"terminal_77","text":"queued question"}))
            .send()
            .await
            .unwrap();
        assert_eq!(queued.status(), 204);
        assert!(state.lock().unwrap().queued.is_some());
        state.lock().unwrap().refuse_stop = true;
        let refused = client
            .post(format!("{}/stop", instance.url))
            .header("origin", &instance.url)
            .header("cookie", &cookie)
            .json(&serde_json::json!({"pane":"terminal_77"}))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), 502);
        assert!(
            refused
                .text()
                .await
                .unwrap()
                .contains("Grok interrupt could not be delivered")
        );
        assert!(!state.lock().unwrap().interrupted);
        {
            let mut state = state.lock().unwrap();
            state.refuse_stop = false;
            state.draft = true;
        }
        let stopped = client
            .post(format!("{}/stop", instance.url))
            .header("origin", &instance.url)
            .header("cookie", &cookie)
            .json(&serde_json::json!({"pane":"terminal_77"}))
            .send()
            .await
            .unwrap();
        assert_eq!(stopped.status(), 204);
        assert!(!state.lock().unwrap().interrupted);
        assert!(!state.lock().unwrap().draft);
        let stopped = client
            .post(format!("{}/stop", instance.url))
            .header("origin", &instance.url)
            .header("cookie", &cookie)
            .json(&serde_json::json!({"pane":"terminal_77"}))
            .send()
            .await
            .unwrap();
        assert_eq!(stopped.status(), 204);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(6);
        while !frames.contains("queued reply after interrupt") {
            let frame = tokio::time::timeout_at(deadline, peer.next())
                .await
                .expect("queued Grok reply frame")
                .unwrap()
                .unwrap();
            frames.push_str(frame.to_text().unwrap());
        }
        assert!(state.lock().unwrap().interrupted);
        assert!(frames.contains("queued question"));
        drop(peer);
        instance.stop().await;
    }

    /// The seed reaches a browser through the RUNNING remote, not just
    /// through a feed: it is published before the server can answer,
    /// so the first browser is told the roster even though the TUI has
    /// not rebuilt since (a-page-that-has-not-heard-is-not-empty).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_remote_answers_its_first_browser_with_what_the_tui_knew() {
        use crate::cli::status_tui::fixtures::{agent_row, snap};
        use clank_core::vocab::AutoMode;
        use futures_util::StreamExt;
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut s = snap(vec![], vec![]);
        s.agents = vec![agent_row(
            "claude",
            crate::cli::teams_config::RosterRole::Master,
            AutoMode::On,
        )];
        // The port this remote remembers lives in the repo's config.
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
        )
        .unwrap();
        let door = Arc::new(door::Door::new(Some(home.path().to_path_buf())));
        let instance = Instance::start(
            repo.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            Arc::new(NoPanes),
            door.clone(),
            Startup {
                tunnel: None,
                poll: std::time::Duration::from_secs(3600),
                grace: std::time::Duration::from_millis(200),
                seed: status_payload(repo.path(), &s, &Default::default()),
            },
        )
        .await
        .expect("a remote with no tunnel");

        let opened = door.open_session("token").unwrap();
        let cookie = format!("{}={opened}", door::COOKIE);
        let host = instance.url.trim_start_matches("http://").to_string();
        let (mut peer, _) =
            tokio_tungstenite::connect_async(socket_to(&host, "/events", Some(&cookie)))
                .await
                .expect("the page's socket");

        // The first frames a page is replayed, before anything has
        // rebuilt: one of them says who is here. Bounded, because what
        // this guards is frames that never come — and a test that
        // waits forever for them reports nothing at all.
        let mut said_who = None;
        for _ in 0..4 {
            let next = tokio::time::timeout(std::time::Duration::from_secs(5), peer.next()).await;
            let Ok(Some(Ok(msg))) = next else {
                break;
            };
            let text = msg.to_text().unwrap_or_default().to_string();
            if let Some(data) = text.split("data: ").nth(1) {
                let v: serde_json::Value = serde_json::from_str(data.trim()).unwrap_or_default();
                if v["facts"]["agents"][0]["label"] == "claude" {
                    said_who = Some(v);
                    break;
                }
            }
        }
        assert!(
            said_who.is_some(),
            "a browser arriving before the first rebuild is told the roster"
        );
        instance.stop().await;
    }

    /// A browser that arrives before the TUI has rebuilt anything is
    /// told the facts anyway: the seed is published before the server
    /// can answer, so the first frames a page is replayed name the
    /// roster. Without it, a page on an idle repo could wait for a
    /// rebuild that never came (a-page-that-has-not-heard-is-not-empty).
    #[test]
    fn a_browser_that_arrives_first_is_still_told_the_facts() {
        use crate::cli::status_tui::fixtures::{agent_row, snap};
        use clank_core::vocab::AutoMode;
        let mut s = snap(vec![], vec![]);
        s.agents = vec![agent_row(
            "claude",
            crate::cli::teams_config::RosterRole::Master,
            AutoMode::On,
        )];
        let seed = status_payload(std::path::Path::new("/nowhere"), &s, &Default::default());

        // As `start` does it: the seed goes in before anything can
        // subscribe, and what a browser is replayed carries it.
        let feed = Feed::new(8);
        feed.status(seed);
        let (frames, _rx) = feed.connect();
        let status = frames
            .iter()
            .find(|f| f.starts_with("event: status\n"))
            .expect("a page arriving first is given the facts");
        let data: serde_json::Value =
            serde_json::from_str(status.split("data: ").nth(1).unwrap().trim()).unwrap();
        assert_eq!(
            data["facts"]["agents"][0]["label"], "claude",
            "the roster the TUI knew, not silence"
        );

        // And a later publication is what a later browser is given.
        let mut newer = snap(vec![], vec![]);
        newer.agents = vec![
            agent_row(
                "claude",
                crate::cli::teams_config::RosterRole::Master,
                AutoMode::On,
            ),
            agent_row(
                "codex",
                crate::cli::teams_config::RosterRole::Commit,
                AutoMode::On,
            ),
        ];
        feed.status(status_payload(
            std::path::Path::new("/nowhere"),
            &newer,
            &Default::default(),
        ));
        let (later, _rx) = feed.connect();
        let status = later
            .iter()
            .find(|f| f.starts_with("event: status\n"))
            .expect("still a status frame");
        let data: serde_json::Value =
            serde_json::from_str(status.split("data: ").nth(1).unwrap().trim()).unwrap();
        assert_eq!(
            data["facts"]["agents"].as_array().unwrap().len(),
            2,
            "the newest facts, not the seed"
        );
    }

    /// The page says it is waiting until it has heard, and says a
    /// session is empty only once something said so. The markup's own
    /// default is the waiting one, because that is what is true before
    /// a line of script has run — which is how a dead script came to
    /// read as an empty repo.
    #[test]
    fn the_page_does_not_call_silence_an_empty_roster() {
        let markup = &PAGE[..PAGE.find("<script nonce").expect("the page's script")];
        assert!(
            markup.contains("Waiting for this session"),
            "the empty note starts as waiting"
        );
        assert!(
            !markup.contains("No agents in this session"),
            "and never claims an empty roster before anything has arrived"
        );
        // Both states exist, and the one that names the roster is
        // reached only through `heard`.
        assert!(PAGE.contains("let heard = false;"));
        assert!(
            PAGE.contains("heard\n      ? 'No agents in this session yet.'")
                || PAGE.contains("heard ? 'No agents in this session yet.'"),
            "the note names an empty roster only when a frame said so"
        );
        assert!(
            PAGE.contains("heard && !roster().length ? 'no agents' : ''"),
            "and the header says it on the same condition"
        );
    }

    /// Both pages are served under a policy, and the policy admits
    /// this origin and nothing else. Removing the links was the
    /// arrangement; this is the enforcement — a page string can be
    /// read for `https://`, but not for what a script does at run
    /// time (codex on b5c90e0).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn both_pages_are_served_under_a_policy() {
        let (base, door, anon, cancel, _site, _repo) = door_only().await;
        let cookie = anon
            .post(format!("{base}/login"))
            .header("origin", &base)
            .json(&serde_json::json!({ "token": door.token().unwrap() }))
            .send()
            .await
            .unwrap()
            .headers()
            .get("set-cookie")
            .expect("a session")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        for (what, path, with_cookie) in [("the page", "/", true), ("sign-in", "/login", false)] {
            let mut ask = anon.get(format!("{base}{path}"));
            if with_cookie {
                ask = ask.header("cookie", &cookie);
            }
            let r = ask.send().await.unwrap();
            assert_eq!(r.status(), 200, "{what}");
            let policy = r
                .headers()
                .get("content-security-policy")
                .unwrap_or_else(|| panic!("{what} is served without a policy"))
                .to_str()
                .unwrap()
                .to_string();
            let html = r.text().await.unwrap();

            // Nothing by default, and every relaxation is this origin
            // or a scheme the page's own content needs.
            assert!(policy.contains("default-src 'none'"), "{what}: {policy}");
            for named in [
                "script-src 'self' 'nonce-",
                "style-src 'self' 'unsafe-inline'",
                "img-src 'self' data:",
                "font-src 'self'",
                "connect-src 'self'",
                "frame-ancestors 'none'",
            ] {
                assert!(policy.contains(named), "{what} wants {named}: {policy}");
            }
            assert!(
                !policy.contains("script-src 'self' 'unsafe-inline'"),
                "{what}: a script is admitted by nonce, not by being inline"
            );

            // The nonce in the header is the one the page's own block
            // carries, and it is fresh each time — one that repeated
            // would admit whatever had seen it once.
            let nonce = policy
                .split("'nonce-")
                .nth(1)
                .and_then(|s| s.split('\'').next())
                .expect("a nonce")
                .to_string();
            assert!(
                html.contains(&format!("<script nonce=\"{nonce}\"")),
                "{what}: the page's script carries the header's nonce"
            );
            assert!(
                !html.contains(NONCE_ANCHOR),
                "{what}: no unreplaced anchor is left in what is served"
            );
            let again = anon
                .get(format!("{base}{path}"))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap();
            assert!(
                !again
                    .headers()
                    .get("content-security-policy")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .contains(&nonce),
                "{what}: a nonce is minted per response"
            );
        }
        let _ = cancel.send(true);
    }

    /// A server with nothing but its door, for the routes that are
    /// only about getting in.
    async fn door_only() -> (
        String,
        Arc<door::Door>,
        reqwest::Client,
        tokio::sync::watch::Sender<bool>,
        tempfile::TempDir,
        tempfile::TempDir,
    ) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let site = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "--quiet", "--initial-branch=main"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(repo.path())
                    .args(&args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let door = Arc::new(door::Door::new(Some(site.path().to_path_buf())));
        tokio::spawn({
            let (repo, pages) = (repo.path().to_path_buf(), site.path().to_path_buf());
            let door = door.clone();
            let sayer: Sayer = Arc::new(|_, _| Ok(()));
            let stopper: Stopper = Arc::new(|_| Ok(()));
            async move {
                serve(
                    listener,
                    Feed::new(16),
                    sayer,
                    stopper,
                    "clank-test".to_string(),
                    repo,
                    pages,
                    cancelled,
                    Arc::new(Blocking::default()),
                    door,
                    "nonce-of-this-start".to_string(),
                    Arc::default(),
                    Arc::default(),
                )
                .await
            }
        });
        let anon = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        (
            format!("http://localhost:{port}"),
            door,
            anon,
            cancel,
            site,
            repo,
        )
    }

    /// The phone's actual way in is the link the TUI mints, which is
    /// redeemed server-side and never touches the sign-in page. Being
    /// signed in is therefore what hands the credential over, and the
    /// credential is what gets back in once the cookie is gone
    /// (the-phone-scans-the-token-and-keeps-it).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_phone_paired_by_the_link_is_handed_the_token_and_keeps_it() {
        let (base, door, anon, cancel, _site, _repo) = door_only().await;

        // Nothing stored and no session: the door hands out nothing.
        assert_eq!(
            anon.get(format!("{base}/token"))
                .send()
                .await
                .unwrap()
                .status(),
            401,
            "the token is not a thing the door gives away"
        );

        // The scan-to-phone link, redeemed as the phone redeems it.
        let link = format!("{base}/login?t={}", door.mint(door::Link::Login));
        let welcome = anon.get(&link).send().await.unwrap();
        assert_eq!(welcome.status(), 303);
        let cookie = welcome
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        // Signed in, the page asks for the credential it will keep.
        let handed = anon
            .get(format!("{base}/token"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(handed.status(), 200);
        let token = handed.json::<serde_json::Value>().await.unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(token, door.token().unwrap());

        // The cookie goes — expired, cleared, a new browser session —
        // and the kept credential is the way back in, with no paste.
        let back = anon
            .post(format!("{base}/login"))
            .header("origin", &base)
            .json(&serde_json::json!({ "token": token }))
            .send()
            .await
            .unwrap();
        assert_eq!(back.status(), 204, "the paste path, as the page posts it");
        let second = back
            .headers()
            .get("set-cookie")
            .expect("a new session")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        assert_ne!(second, cookie, "a new session, not the old one");
        assert_eq!(
            anon.get(format!("{base}/whoami"))
                .header("cookie", &second)
                .send()
                .await
                .unwrap()
                .status(),
            204
        );
        let _ = cancel.send(true);
    }

    /// The page keeps the credential unless the door says the
    /// CREDENTIAL was refused. 403 alone cannot say that — a request
    /// from another origin is refused with it too — and a rate limit
    /// or a store failure is this minute's problem, not a reason to
    /// throw away a working token (codex on 1c932ab).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn only_a_refused_credential_is_marked_as_one() {
        let (base, _door, anon, cancel, _site, _repo) = door_only().await;
        let wrong = anon
            .post(format!("{base}/login"))
            .header("origin", &base)
            .json(&serde_json::json!({ "token": "not-it" }))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), 403);
        assert_eq!(
            wrong.json::<serde_json::Value>().await.unwrap()["refused"],
            "token",
            "the one refusal that means: stop keeping this"
        );

        let elsewhere = anon
            .post(format!("{base}/login"))
            .json(&serde_json::json!({ "token": "not-it" }))
            .send()
            .await
            .unwrap();
        assert_eq!(elsewhere.status(), 403, "same status");
        assert!(
            !elsewhere.text().await.unwrap().contains("\"refused\""),
            "and no mark, so a page cannot read it as a rotation"
        );
        let _ = cancel.send(true);
    }

    /// Forgetting ends the session as well as the store: a live cookie
    /// would fetch the token again on the next load, and the device
    /// could never stay forgotten.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forgetting_a_device_signs_it_out() {
        let (base, door, anon, cancel, _site, _repo) = door_only().await;
        let token = door.token().unwrap();
        let sign_in = || async {
            anon.post(format!("{base}/login"))
                .header("origin", &base)
                .json(&serde_json::json!({ "token": token }))
                .send()
                .await
                .unwrap()
                .headers()
                .get("set-cookie")
                .expect("a session")
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_string()
        };
        let cookie = sign_in().await;

        assert_eq!(
            anon.post(format!("{base}/forget"))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap()
                .status(),
            403,
            "not from this page"
        );
        assert_eq!(
            anon.post(format!("{base}/forget"))
                .header("origin", &base)
                .send()
                .await
                .unwrap()
                .status(),
            401,
            "and not from nowhere"
        );

        let gone = anon
            .post(format!("{base}/forget"))
            .header("origin", &base)
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(gone.status(), 204);
        let spent = gone
            .headers()
            .get("set-cookie")
            .expect("the cookie is spent")
            .to_str()
            .unwrap()
            .to_string();
        assert!(spent.contains("Max-Age=0"), "{spent}");
        assert_eq!(
            anon.get(format!("{base}/whoami"))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap()
                .status(),
            401,
            "the session it was signed in on is over"
        );
        assert_eq!(
            anon.get(format!("{base}/token"))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap()
                .status(),
            401,
            "so nothing re-provisions it"
        );

        // Signing in again is the explicit choice that remembers it.
        let after = sign_in().await;
        assert_eq!(
            anon.get(format!("{base}/token"))
                .header("cookie", &after)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        let _ = cancel.send(true);
    }
}

/// The credential's lifecycle, DRIVEN.
///
/// Every bug this module exists to stop was about order, so every test
/// here is a SEQUENCE: events dispatched one after another, each
/// reading the state the last one left. Supplying `tried` or
/// `sealed` by hand would answer for a state the page can reach
/// only by a path nothing checks, which is the gap codex found in the
/// predicate-shaped version of these tests (fd3ae49).
#[cfg(test)]
mod door_js_tests {
    use super::{COMPOSER_JS, DOOR_JS, login_page, page_for};

    /// Runs the SHIPPED module, threads `events` through `doorStep`
    /// from a starting environment, and returns the JSON of every
    /// step's effects plus the final state.
    ///
    /// `DOOR_JS` is the same constant both pages are rendered with, so
    /// a decision that changes here changes there; a harness carrying
    /// its own copy of the logic would pass while the pages shipped
    /// something else.
    fn drive(env: &str, events: &[&str]) -> serde_json::Value {
        let script = format!(
            r#"{DOOR_JS}
            (() => {{
              let state = doorStart({env});
              const steps = [];
              for (const ev of [{}]) {{
                const r = doorStep(state, ev);
                state = r.state;
                steps.push(r.effects);
              }}
              return JSON.stringify({{ steps, state }});
            }})()"#,
            events.join(", ")
        );
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        let raw: String = ctx.with(|ctx| match ctx.eval::<String, _>(script) {
            Ok(v) => v,
            Err(e) => {
                let detail = ctx.catch();
                panic!("the door module did not run: {e} {detail:?}");
            }
        });
        serde_json::from_str(&raw).unwrap()
    }

    /// The effect kinds one step produced, in order.
    fn dos(step: &serde_json::Value) -> Vec<String> {
        step.as_array()
            .unwrap()
            .iter()
            .map(|f| f["do"].as_str().unwrap().to_string())
            .collect()
    }

    /// A sign-in that SUCCEEDS navigates and spends the guard — and
    /// the guard then stops the next load asking again, which is the
    /// loop between the two pages this exists to break.
    #[test]
    fn a_sign_in_that_succeeds_navigates_and_then_does_not_repeat() {
        let r = drive(
            "{stored:'t', tried:false}",
            &[
                "{t:'load'}",
                "{t:'signInReply', reply:{ok:true, status:204, body:null}}",
                "{t:'load'}",
            ],
        );
        assert_eq!(dos(&r["steps"][0]), ["signIn"], "a stored token signs in");
        assert_eq!(
            dos(&r["steps"][1]),
            ["guard", "navigate"],
            "success spends the guard and goes"
        );
        assert_eq!(r["steps"][1][0]["set"], true);
        assert_eq!(r["steps"][1][1]["to"], "/");
        assert!(
            dos(&r["steps"][2]).is_empty(),
            "the spent guard stops the next load: this is the loop"
        );
    }

    /// A recoverable failure leaves the guard alone, so the NEXT load
    /// tries again. Spending it here would strand a browser that only
    /// hit a bad minute at the form.
    #[test]
    fn a_recoverable_failure_is_retried_on_the_next_load() {
        for reply in [
            "{ok:false, status:429, body:null}",
            "{ok:false, status:500, body:null}",
            "{ok:false, status:403, body:null}",
            "{ok:false, status:403, body:{refused:'origin'}}",
            "null",
        ] {
            let r = drive(
                "{stored:'t', tried:false}",
                &[
                    "{t:'load'}",
                    &format!("{{t:'signInReply', reply:{reply}}}"),
                    "{t:'load'}",
                ],
            );
            let said = dos(&r["steps"][1]);
            assert_eq!(said, ["say"], "{reply}: says so and keeps everything");
            assert_eq!(
                r["state"]["stored"], "t",
                "{reply}: the credential survives a bad minute"
            );
            assert_eq!(
                dos(&r["steps"][2]),
                ["signIn"],
                "{reply}: the next load must try again"
            );
        }
    }

    /// Only a refusal that NAMES the credential discards it, and a
    /// discarded credential does not sign in again.
    #[test]
    fn a_refusal_of_the_credential_discards_it_and_stops_trying() {
        let r = drive(
            "{stored:'t', tried:false}",
            &[
                "{t:'load'}",
                "{t:'signInReply', reply:{ok:false, status:403, body:{refused:'token'}}}",
                "{t:'load'}",
            ],
        );
        assert_eq!(dos(&r["steps"][1]), ["discard", "say"]);
        assert_eq!(r["steps"][1][1]["why"], "dead");
        assert!(r["state"]["stored"].is_null());
        assert!(
            dos(&r["steps"][2]).is_empty(),
            "nothing is stored, so there is nothing to sign in with"
        );
    }

    /// Arriving signed in releases the guard, so a cookie lost LATER
    /// is signed back in rather than left at the form.
    #[test]
    fn arriving_signed_in_releases_the_guard_for_a_later_loss() {
        let r = drive(
            "{stored:'t', tried:true}",
            &["{t:'load'}", "{t:'signedIn'}", "{t:'load'}"],
        );
        assert!(
            dos(&r["steps"][0]).is_empty(),
            "the guard was spent, so this load does not ask"
        );
        assert_eq!(dos(&r["steps"][1]), ["guard"]);
        assert_eq!(r["steps"][1][0]["set"], false);
        assert_eq!(
            dos(&r["steps"][2]),
            ["signIn"],
            "released: a cookie lost later signs back in"
        );
    }

    /// A sign-out the door REFUSED keeps everything — and the button
    /// works again afterwards, which is what `retry` has to mean.
    #[test]
    fn a_forget_the_door_refused_keeps_everything_and_can_be_repeated() {
        let r = drive(
            "{stored:'t', tried:false}",
            &[
                "{t:'forget'}",
                "{t:'forgetReply', ok:false}",
                "{t:'forget'}",
                "{t:'forgetReply', ok:true}",
            ],
        );
        assert_eq!(dos(&r["steps"][0]), ["forget"]);
        assert_eq!(dos(&r["steps"][1]), ["say"], "a failure discards nothing");
        assert_eq!(r["steps"][1][0]["why"], "forgetFailed");
        assert_eq!(
            dos(&r["steps"][2]),
            ["forget"],
            "and the button is live again"
        );
        assert_eq!(dos(&r["steps"][3]), ["discard", "navigate"]);
        assert_eq!(r["steps"][3][1]["to"], "/login");
        assert!(r["state"]["stored"].is_null());
    }

    /// A press while a sign-out is in flight is not a second sign-out.
    #[test]
    fn a_second_press_during_a_forget_does_nothing() {
        let r = drive("{stored:'t'}", &["{t:'forget'}", "{t:'forget'}"]);
        assert_eq!(dos(&r["steps"][0]), ["forget"]);
        assert!(dos(&r["steps"][1]).is_empty());
    }

    /// THE RACE: the provisioning fetch was in flight when the forget
    /// began, so its reply lands after. It must write nothing.
    ///
    /// Reached by dispatching the forget BETWEEN the two, rather than
    /// by handing the step a `sealed` flag — a page that captured
    /// the flag early and decided on it late is the bug, and this
    /// ordering is the only thing that can tell the two apart.
    #[test]
    fn a_provision_that_lands_after_a_forget_begins_writes_nothing() {
        let r = drive(
            "{stored:null}",
            &[
                "{t:'forget'}",
                "{t:'provisionReply', token:'late'}",
                "{t:'forgetReply', ok:true}",
            ],
        );
        assert!(
            dos(&r["steps"][1]).is_empty(),
            "the forget was racing this arrival and won"
        );
        assert!(
            r["state"]["stored"].is_null(),
            "nothing may be kept across a sign-out"
        );
    }

    /// THE RACE, PAST THE SIGN-OUT: the seal outlives a forget that
    /// SUCCEEDED, because navigating does not stop a promise that was
    /// already in flight from resolving first.
    ///
    /// Every other sequence dispatches the late reply before the
    /// door answers, so this is the one transition that pins the seal
    /// as terminal. Without it, adding `s.sealed = false` to the ok
    /// branch — which reads as ordinary tidying-up — passes every
    /// other test in this module and puts the credential back after
    /// the user signed out (ruthless a5504e9).
    #[test]
    fn a_provision_that_lands_after_a_completed_sign_out_writes_nothing() {
        let r = drive(
            "{stored:'t'}",
            &[
                "{t:'forget'}",
                "{t:'forgetReply', ok:true}",
                "{t:'provisionReply', token:'late'}",
            ],
        );
        assert_eq!(dos(&r["steps"][1]), ["discard", "navigate"]);
        assert!(
            dos(&r["steps"][2]).is_empty(),
            "the sign-out is done and the seal holds: nothing may be written back"
        );
        assert!(
            r["state"]["stored"].is_null(),
            "the credential must not survive a completed sign-out"
        );
        assert_eq!(
            r["state"]["sealed"], true,
            "the seal is terminal on success -- clearing it re-arms the race"
        );
    }

    /// The same reply with no forget in flight is kept: the race is
    /// what stops it, not the reply.
    #[test]
    fn a_provision_that_lands_before_any_forget_is_kept() {
        let r = drive("{stored:null}", &["{t:'provisionReply', token:'fresh'}"]);
        assert_eq!(dos(&r["steps"][0]), ["store"]);
        assert_eq!(r["steps"][0][0]["token"], "fresh");
        assert_eq!(r["state"]["stored"], "fresh");
    }

    /// Runs the shipped COMPOSER module and returns `expr`'s value.
    fn composer<T>(expr: &str) -> T
    where
        T: for<'js> rquickjs::FromJs<'js>,
    {
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(
            |ctx| match ctx.eval::<T, _>(format!("{COMPOSER_JS}\n{expr}")) {
                Ok(v) => v,
                Err(e) => {
                    let detail = ctx.catch();
                    panic!("`{expr}` did not run: {e} {detail:?}");
                }
            },
        )
    }

    /// Enter sends where there is a keyboard, and inserts where the
    /// return key IS the newline key.
    ///
    /// A blanket unshifted-Enter send leaves a phone unable to type a
    /// second line at all, and this page is mostly read on one
    /// (codex a664d6c).
    #[test]
    fn enter_sends_on_a_keyboard_and_writes_a_newline_on_a_phone() {
        let key = |ev: &str| composer::<String>(&format!("composerKey({ev})"));
        assert_eq!(key("{key:'Enter', hasKeyboard:true}"), "send");
        assert_eq!(
            key("{key:'Enter', hasKeyboard:false}"),
            "newline",
            "a touch keyboard's return must still make a line",
        );
        assert_eq!(
            key("{key:'Enter', shiftKey:true, hasKeyboard:true}"),
            "newline"
        );
        // Every other key is the textarea's own business.
        for other in ["{key:'a', hasKeyboard:true}", "{key:'Escape'}", "null"] {
            assert_eq!(key(other), "pass", "{other}");
        }
    }

    /// An Enter that CONFIRMS an IME composition never sends.
    ///
    /// The composer is choosing characters, not finishing a message;
    /// sending there cuts a sentence off mid-word in every language
    /// that needs one. Both signals, because older engines report the
    /// legacy keyCode and no `isComposing`.
    #[test]
    fn an_ime_confirmation_is_not_a_send() {
        let key = |ev: &str| composer::<String>(&format!("composerKey({ev})"));
        assert_eq!(
            key("{key:'Enter', isComposing:true, hasKeyboard:true}"),
            "pass"
        );
        assert_eq!(key("{key:'Enter', keyCode:229, hasKeyboard:true}"), "pass");
    }

    /// The box grows with its content, stops at a third of the
    /// viewport, and SHRINKS back.
    #[test]
    fn the_box_grows_to_a_cap_and_shrinks_again() {
        let h = |m: &str| composer::<i64>(&format!("composerHeight({m})"));
        let viewport = "viewportPx:900, lineHeightPx:20";
        assert_eq!(
            h(&format!("{{contentPx:0, {viewport}}}")),
            20,
            "one row at rest"
        );
        assert_eq!(h(&format!("{{contentPx:20, {viewport}}}")), 20);
        assert_eq!(
            h(&format!("{{contentPx:140, {viewport}}}")),
            140,
            "grows with content"
        );
        assert_eq!(
            h(&format!("{{contentPx:5000, {viewport}}}")),
            300,
            "a third of 900"
        );
        // The shrink is the same function of the content, not a
        // maximum anything remembers: a box that only grew would be
        // the reported bug pointing the other way.
        assert_eq!(h(&format!("{{contentPx:40, {viewport}}}")), 40);

        // The cap follows the VIEWPORT, which changes without an
        // input event -- a phone rotating, a keyboard opening.
        assert_eq!(h("{contentPx:5000, viewportPx:300, lineHeightPx:20}"), 100);
        // And never below one row, however small the viewport gets.
        assert_eq!(h("{contentPx:5000, viewportPx:10, lineHeightPx:20}"), 20);
    }

    /// It scrolls only once it has stopped growing.
    #[test]
    fn the_box_scrolls_only_at_its_cap() {
        let s = |m: &str| composer::<bool>(&format!("composerScrolls({m})"));
        assert!(!s("{contentPx:140, viewportPx:900, lineHeightPx:20}"));
        assert!(s("{contentPx:5000, viewportPx:900, lineHeightPx:20}"));
    }

    /// Every write to the box resizes it, not only the ones a person
    /// types.
    ///
    /// Setting `.value` fires no `input` event, so the attach, the
    /// dispatch clear and the failed-send restore each have to say so
    /// themselves -- and the window resize has no input event either.
    /// Asserted on the RENDERED page, since that is what ships.
    #[test]
    fn every_write_to_the_box_resizes_it() {
        let page = page_for("s");
        let body = COMPOSER_JS.trim();
        let wiring = page.replace(body, "");
        assert!(
            !wiring.contains("function composerHeight"),
            "the module was not cut out, so this proves nothing"
        );
        for (what, write) in [
            (
                "the attach",
                "input.value = input.value ? `${input.value} ${path}` : path;",
            ),
            ("the dispatch clear", "input.value = '';"),
            (
                "the failed-send restore",
                "input.value = input.value ? `${text} ${input.value}` : text;",
            ),
        ] {
            let at = wiring
                .find(write)
                .unwrap_or_else(|| panic!("{what} still writes the box"));
            let after = &wiring[at + write.len()..];
            let next = after.find("resize()").unwrap_or(usize::MAX);
            assert!(next < 120, "{what} writes the box and does not resize it",);
        }
        assert!(
            wiring.contains("window.addEventListener('resize', resize)"),
            "a viewport change re-wraps the text and moves the cap",
        );
        assert!(
            wiring.contains("input.addEventListener('input', resize)"),
            "and typing resizes it",
        );
    }

    /// The box is sized against the viewport the reader can SEE.
    ///
    /// A software keyboard shrinks only the visual viewport, leaving
    /// `innerHeight` at its pre-keyboard value -- so sizing against
    /// that keeps a cap computed for a screen that is no longer
    /// visible, while typing, on a phone. Which is the whole case
    /// this box exists for (codex d36f50e).
    #[test]
    fn the_cap_follows_the_viewport_the_keyboard_left() {
        let v = |win: &str| composer::<i64>(&format!("composerViewport({win})"));
        assert_eq!(
            v("{innerHeight:900, visualViewport:{height:300}}"),
            300,
            "the keyboard is open: 300 is what is left to see",
        );
        // Fallbacks, in order, for a browser without the API.
        assert_eq!(v("{innerHeight:900}"), 900);
        assert_eq!(v("{innerHeight:900, visualViewport:null}"), 900);
        assert_eq!(v("null"), 0);
        // A reported height of ZERO is treated as absent, DELIBERATELY:
        // a cap of nothing would collapse the box. Today that is
        // operator precedence rather than a decision anyone wrote, so
        // this line is what makes it one (ruthless 7ef1088).
        assert_eq!(v("{innerHeight:900, visualViewport:{height:0}}"), 900);

        // End to end through the sizing: the cap moves with it.
        let keyboard_shut = "{contentPx:5000, lineHeightPx:20, \
            viewportPx:composerViewport({innerHeight:900, visualViewport:{height:900}})}";
        let keyboard_open = "{contentPx:5000, lineHeightPx:20, \
            viewportPx:composerViewport({innerHeight:900, visualViewport:{height:300}})}";
        assert_eq!(
            composer::<i64>(&format!("composerHeight({keyboard_shut})")),
            300
        );
        assert_eq!(
            composer::<i64>(&format!("composerHeight({keyboard_open})")),
            100,
            "with the keyboard open the box must give back the room",
        );
    }

    /// And the page ASKS for that viewport, and listens to it.
    ///
    /// The sizing test supplies `viewportPx` itself, so it cannot
    /// catch the page handing over `innerHeight` -- the adapter is
    /// where this bug lived.
    #[test]
    fn the_page_reads_and_watches_the_visual_viewport() {
        let wiring = page_for("s").replace(COMPOSER_JS.trim(), "");
        assert!(
            !wiring.contains("function composerViewport"),
            "the module was not cut out, so this proves nothing"
        );
        assert!(
            wiring.contains("viewportPx: composerViewport(window)"),
            "the page must ask the module which viewport to size against",
        );
        assert!(
            !wiring.contains("viewportPx: window.innerHeight"),
            "innerHeight does not change when a keyboard opens",
        );
        assert!(
            wiring.contains("window.visualViewport.addEventListener('resize', resize)"),
            "a keyboard opening resizes the VISUAL viewport and not the window, \
             so the window's event never fires for it",
        );
    }

    /// The composer's decisions reach the page through the module,
    /// not a second copy.
    #[test]
    fn the_page_asks_the_composer_module_it_ships() {
        let page = page_for("s");
        let wiring = page.replace(COMPOSER_JS.trim(), "");
        assert!(
            wiring.contains("composerKey({"),
            "the keydown handler must ask the module"
        );
        assert!(wiring.contains("composerHeight(m)"));
        assert!(wiring.contains("composerScrolls(m)"));
        // And the element is one that can grow downward at all.
        assert!(
            page.contains("<textarea id=\"text\""),
            "an <input> is one line forever, which is the bug",
        );
    }

    /// The module the tests run is the module the pages ship. Without
    /// this, every sequence above could hold of a file no page loads.
    #[test]
    fn both_pages_ship_the_module_these_tests_run() {
        let body = DOOR_JS.trim();
        for (name, page) in [("page.html", page_for("s")), ("login.html", login_page())] {
            assert!(
                page.contains(body),
                "{name} must carry the module verbatim, or these verdicts are about nothing"
            );
        }
    }

    /// And each page DRIVES it.
    ///
    /// Carrying the module proves nothing about using it: a page that
    /// decides for itself passes the test above while every sequence
    /// here describes code nothing runs. So the module body is cut out
    /// and the wiring that remains must dispatch the events that page
    /// is responsible for.
    #[test]
    fn each_page_dispatches_the_events_it_is_responsible_for() {
        let body = DOOR_JS.trim();
        for (name, page, wanted) in [
            ("login.html", login_page(), &["load", "signInReply"][..]),
            (
                "page.html",
                page_for("s"),
                &["signedIn", "forget", "forgetReply", "provisionReply"][..],
            ),
        ] {
            let wiring = page.replace(body, "");
            assert!(
                !wiring.contains("function doorStep"),
                "{name}: the module was not cut out, so this proves nothing"
            );
            assert!(
                wiring.contains("doorStep(door, event)"),
                "{name} must step the machine it ships"
            );
            for ev in wanted {
                assert!(
                    wiring.contains(&format!("t: '{ev}'")),
                    "{name} never dispatches `{ev}`"
                );
            }
        }
    }
}
