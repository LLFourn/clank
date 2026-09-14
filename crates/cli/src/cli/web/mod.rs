//! `clank web` — one repo's agents in a browser, one pane at a time.
//!
//! `zellij web` shares a session at ONE geometry, sized to its
//! smallest client, so a phone gets the desktop's tab or shrinks the
//! desktop. This subscribes to each agent pane on its own instead:
//! the viewport arrives independent of the layout, and the page shows
//! one agent at a time. Terminals do not reflow, so a pane arrives at
//! its desktop size; the page scrolls and pinches around it.
//!
//! Localhost, no authentication: a proof of concept
//! (clank-web-shows-each-agent-on-a-phone).

mod feed;
mod transcript;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Bytes, Frame};

use super::{WebArgs, resolve_repo};
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

pub async fn run(args: WebArgs) -> anyhow::Result<()> {
    let shutdown = Shutdown::arm(args.attached_to)?;
    let repo = resolve_repo(args.repo.as_deref())?;
    open_zellij::require_zellij()?;
    let session = choose_session(
        args.session.as_deref(),
        open_zellij::current_session().as_deref(),
        &repo,
    );
    let panes = open_zellij::snapshot_panes_in(&session).ok_or_else(|| {
        anyhow::anyhow!(
            "zellij did not answer for session `{session}` — is it running? \
             (`zellij list-sessions`; pass --session to name another)"
        )
    })?;
    if open_zellij::repo_tab_id(&panes, &repo).is_none() {
        anyhow::bail!(
            "session `{session}` holds none of {}'s panes — no status pane, no agent pane. \
             `clank open` puts a repo's tab in whatever session you were in; find it with \
             `zellij list-sessions` and pass --session.",
            repo.display()
        );
    }

    // The port before any child: a bind that fails returns from here
    // with nothing to clean up. It once came after the subscription,
    // and the error path left a `zellij subscribe` streaming to nobody
    // — the poll task's clone of it outlived the function, and the
    // process ended without dropping it (an occupied port, live).
    let (listener, port) = match listen(&repo, args.port).await? {
        Listen::Bound { listener, port } => (listener, port),
        Listen::AlreadyOn { identity } => {
            println!(
                "clank web: already on http://127.0.0.1:{}  (pid {}, attached to {})",
                identity.port,
                identity.pid,
                identity
                    .attached_to
                    .map_or("nobody".to_string(), |m| m.to_string())
            );
            return Ok(());
        }
    };
    let identity = Identity {
        clank: "web".to_string(),
        repo: canonical(&repo),
        session: session.clone(),
        pid: std::process::id(),
        port,
        attached_to: args.attached_to,
    };
    let feed = Feed::new(64);
    let table = open_zellij::agent_pane_table(&panes, &repo);
    feed.panes(table.clone());
    let armed = shutdown.witness();
    let subscription = Arc::new(Mutex::new(start_subscription(
        armed, &session, &table, &feed,
    )));

    spawn_status_loop(repo.clone(), feed.clone());
    spawn_pane_poll(
        armed,
        session.clone(),
        repo.clone(),
        feed.clone(),
        subscription.clone(),
    );

    println!(
        "clank web: http://127.0.0.1:{port}  (session {session}, {} agent pane{})",
        table.len(),
        if table.len() == 1 { "" } else { "s" }
    );
    let say_session = session.clone();
    let sayer: Sayer =
        Arc::new(move |pane, text| open_zellij::say_to_pane(&say_session, pane, text));
    // A signal runs no destructors, so the subscription child — a
    // `zellij subscribe` that would otherwise stream to nobody for as
    // long as the session lives — is dropped here on purpose before
    // the process ends. The smoke test that found this left one.
    let site = site_dir(&repo);
    let served = tokio::select! {
        r = serve(listener, feed, sayer, &session, repo.clone(), site, identity) => r,
        _ = shutdown.asked() => Ok(()),
    };
    drop(
        subscription
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take(),
    );
    served
}

/// Ctrl-C, or SIGTERM where there is one — a pane closing, a `pkill`.
/// Who this server is, for whoever finds its port taken: `GET
/// /identity`. `repo` is canonical so two spellings of one path
/// compare equal; `attached_to` is the process it ends with.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Identity {
    clank: String,
    repo: String,
    session: String,
    pid: u32,
    port: u16,
    #[serde(default)]
    attached_to: Option<u32>,
}

fn canonical(repo: &Path) -> String {
    repo.canonicalize()
        .unwrap_or_else(|_| repo.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

enum Listen {
    Bound {
        listener: tokio::net::TcpListener,
        port: u16,
    },
    /// This repo's server is up already; nothing to start.
    AlreadyOn { identity: Identity },
}

/// Where to listen. `explicit` means exactly that port and records
/// nothing. Otherwise the port the repo remembers; when that will
/// not bind, whoever holds it is asked: this repo's own server is
/// reported as already on, anything else — another repo's, a
/// stranger, a port that answers nothing — has the number now, so a
/// fresh one is sampled from the OS, remembered in the repo's
/// config, and bound (a-repo-remembers-its-port).
async fn listen(repo: &Path, explicit: Option<u16>) -> anyhow::Result<Listen> {
    let bind = |port: u16| tokio::net::TcpListener::bind(("127.0.0.1", port));
    if let Some(port) = explicit {
        let listener = bind(port)
            .await
            .map_err(|e| anyhow::anyhow!("cannot listen on 127.0.0.1:{port}: {e}"))?;
        return Ok(Listen::Bound { listener, port });
    }
    if let Some(port) = crate::agent_store::web_port(repo)? {
        if let Ok(listener) = bind(port).await {
            return Ok(Listen::Bound { listener, port });
        }
        if let Some(identity) = identity_at(port).await
            && identity.repo == canonical(repo)
        {
            return Ok(Listen::AlreadyOn { identity });
        }
    }
    let listener = bind(0)
        .await
        .map_err(|e| anyhow::anyhow!("cannot listen on 127.0.0.1: {e}"))?;
    let port = listener.local_addr()?.port();
    crate::agent_store::record_web_port(repo, port)?;
    Ok(Listen::Bound { listener, port })
}

/// Whatever answers `/identity` on `port` as a clank server, within
/// a second; anything else is `None`.
async fn identity_at(port: u16) -> Option<Identity> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(1))
        .build()
        .ok()?;
    let identity: Identity = client
        .get(format!("http://127.0.0.1:{port}/identity"))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    (identity.clank == "web").then_some(identity)
}

/// The ways this server is asked to stop — SIGTERM, Ctrl-C, the
/// process it is attached to going away — armed BEFORE any child of
/// its own exists. A signal handler is installed when its stream is
/// first created; until then SIGTERM is the default action, which
/// runs no destructor and would leave a `zellij subscribe` streaming
/// to nobody. So the streams are created here, first, and whatever
/// spawns a child takes this as its witness (codex on 1764612).
struct Shutdown {
    #[cfg(unix)]
    term: Option<tokio::signal::unix::Signal>,
    attached_to: Option<u32>,
}

/// Proof that [`Shutdown::arm`] has run: only it makes one, and
/// whatever spawns a child of this server's takes one.
#[derive(Clone, Copy)]
struct Armed(());

impl Shutdown {
    fn witness(&self) -> Armed {
        Armed(())
    }

    fn arm(attached_to: Option<u32>) -> anyhow::Result<Self> {
        #[cfg(unix)]
        let term = {
            use tokio::signal::unix::{SignalKind, signal};
            // A runtime without signal support still gets Ctrl-C
            // and the attached pid; it is not a reason to refuse.
            signal(SignalKind::terminate()).ok()
        };
        Ok(Self {
            #[cfg(unix)]
            term,
            attached_to,
        })
    }

    async fn asked(mut self) {
        #[cfg(unix)]
        let term = async {
            match self.term.as_mut() {
                Some(t) => {
                    t.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        #[cfg(not(unix))]
        let term = std::future::pending::<()>();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term => {}
            _ = attached_gone(self.attached_to) => {}
        }
    }
}

/// Resolves once the process this server is attached to is gone;
/// never, when it is attached to none. `kill(pid, 0)` rather than a
/// parent check: the TUI spawns the server, but nothing says it stays
/// the parent for the server's whole life.
async fn attached_gone(pid: Option<u32>) {
    let Some(pid) = pid else {
        return std::future::pending().await;
    };
    loop {
        if !process_exists(pid) {
            return;
        }
        tokio::time::sleep(ATTACHED_POLL).await;
    }
}

const ATTACHED_POLL: std::time::Duration = std::time::Duration::from_secs(2);

fn process_exists(pid: u32) -> bool {
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    // Another user's process answers EPERM and is just as alive.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Start (or restart) the one subscription child for `table`'s live
/// panes, and the thread that reads it into the feed. The previous
/// child, if any, is the caller's to drop — dropping kills it, and
/// its reader thread ends on EOF.
/// `_armed`: no child of this server's before its shutdown is armed
/// — see [`Shutdown`].
fn start_subscription(
    _armed: Armed,
    session: &str,
    table: &[PaneMeta],
    feed: &Feed,
) -> Option<SubscribeChild> {
    let ids: Vec<String> = table
        .iter()
        .filter(|p| !p.exited)
        .map(|p| p.id.clone())
        .collect();
    let mut child = open_zellij::subscribe_panes(session, &ids)?;
    let stdout = child.take_stdout()?;
    let feed = feed.clone();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(ev) = serde_json::from_str::<SubscribeEvent>(&line) {
                feed.event(ev);
            }
        }
    });
    Some(child)
}

/// The status feed: the TUI's own watcher wakes a rebuild, and the
/// rebuilt snapshot goes out with the TUI's derived facts. The same
/// thread follows each agent's transcript, because which transcripts
/// exist is a fact of the rebuilt snapshot: an agent rebound to a new
/// session gets a new tail and a new window.
///
/// One thread owns the watcher, its wake channel, and the rebuild —
/// on a runtime of its own. `build_async` holds a `git_io::Repo`
/// across its awaits, which is not `Send`, so it cannot be spawned
/// onto the server's runtime; the TUI never spawns it either. Nothing
/// non-`Send` crosses a thread this way: only the `Feed` does.
fn spawn_status_loop(repo: PathBuf, feed: Feed) {
    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let Ok(_watchers) = crate::cli::status::watch_status_paths(tx, &repo) else {
            return;
        };
        let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
        else {
            return;
        };
        let basename = repo
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("repo")
            .to_string();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut tails: std::collections::HashMap<String, TailHandle> = Default::default();
        let mut next_owner: u64 = 0;
        loop {
            let built = rt.block_on(crate::cli::status::StatusSnapshot::build_async(
                &repo,
                &basename,
                home.as_deref(),
                crate::rebuild::CachePolicy::Use,
                None,
                true,
            ));
            if let Ok(snap) = built {
                // The pages the ledger links, from the same builder
                // `clank html` runs — incremental, and off the
                // request path.
                if let Err(e) = rt.block_on(crate::cli::html::generate(
                    &repo,
                    &site_dir(&repo),
                    home.as_deref(),
                    false,
                )) {
                    eprintln!("clank web: site build failed: {e:#}");
                }
                let with = reconcile_tails(
                    &repo,
                    home.as_deref(),
                    &snap,
                    &feed,
                    &mut tails,
                    &mut next_owner,
                );
                feed.status(serde_json::json!({
                    "facts": crate::cli::status_tui::web_facts(&snap, &with),
                    "snapshot": snap.to_json(),
                }));
            }
            if rx.recv().is_err() {
                break;
            }
            // A burst of wakes is one rebuild.
            while rx
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_ok()
            {}
        }
    });
}

/// A transcript being followed for one agent, for one session, from
/// one file. The stop flag is a courtesy; the token the feed's window
/// records for the tail is the guarantee.
struct TailHandle {
    session: String,
    path: PathBuf,
    stop: Arc<std::sync::atomic::AtomicBool>,
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
    retarget_tails(&wanted, &roster, feed, tails, next_owner)
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
    next_owner: &mut u64,
) -> std::collections::BTreeSet<String> {
    let mut retired = Vec::new();
    tails.retain(|label, h| {
        let same = wanted
            .get(label)
            .is_some_and(|(sid, path)| sid == &h.session && path == &h.path);
        if !same && !wanted.contains_key(label) {
            retired.push(label.clone());
        }
        same
    });
    for label in retired {
        *next_owner += 1;
        feed.claim_turns(&label, *next_owner, None);
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
        std::thread::spawn(move || run_tail(feed, label2, owner, session2, path2, stop2));
        tails.insert(
            label.clone(),
            TailHandle {
                session: session.clone(),
                path: path.clone(),
                stop,
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
const PANE_POLL: std::time::Duration = std::time::Duration::from_secs(4);

fn spawn_pane_poll(
    armed: Armed,
    session: String,
    repo: PathBuf,
    feed: Feed,
    subscription: Arc<Mutex<Option<SubscribeChild>>>,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(PANE_POLL).await;
            let s = session.clone();
            let Ok(Some(panes)) =
                tokio::task::spawn_blocking(move || open_zellij::snapshot_panes_in(&s)).await
            else {
                continue;
            };
            let table = open_zellij::agent_pane_table(&panes, &repo);
            if feed.panes(table.clone()) == TableChange::Membership {
                let fresh = start_subscription(armed, &session, &table, &feed);
                *subscription.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
            }
        }
    });
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

async fn serve(
    listener: tokio::net::TcpListener,
    feed: Feed,
    sayer: Sayer,
    session: &str,
    repo: PathBuf,
    site: PathBuf,
    identity: Identity,
) -> anyhow::Result<()> {
    let pages = Arc::new(Pages {
        repo,
        site,
        identity,
    });
    let page: Arc<str> = Arc::from(PAGE.replace(
        "window.CLANK_SESSION || 'clank'",
        &format!("{}", serde_json::json!(session)),
    ));
    loop {
        let (stream, _) = listener.accept().await?;
        let io = hyper_util::rt::TokioIo::new(stream);
        let (feed, sayer, page, pages) = (feed.clone(), sayer.clone(), page.clone(), pages.clone());
        let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
            let (feed, sayer, page, pages) =
                (feed.clone(), sayer.clone(), page.clone(), pages.clone());
            async move {
                Ok::<_, std::convert::Infallible>(route(req, &feed, &sayer, &page, &pages).await)
            }
        });
        tokio::spawn(async move {
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, svc)
                .await;
        });
    }
}

/// Where the site's pages come from: the built site, and the repo
/// for a commit page the site never builds — and who is serving.
struct Pages {
    repo: PathBuf,
    site: PathBuf,
    identity: Identity,
}

async fn route(
    req: hyper::Request<hyper::body::Incoming>,
    feed: &Feed,
    sayer: &Sayer,
    page: &str,
    pages: &Pages,
) -> Resp {
    match (req.method().as_str(), req.uri().path()) {
        ("GET", "/") => text(200, page.to_string(), "text/html; charset=utf-8"),
        ("GET", "/events") => events(feed),
        ("POST", "/say") => say(req, sayer).await,
        ("GET", "/identity") => text(
            200,
            serde_json::to_string(&pages.identity).unwrap_or_default(),
            "application/json",
        ),
        ("GET", path) if path.starts_with("/html/") => site_page(pages, &path["/html/".len()..]),
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
fn events(feed: &Feed) -> Resp {
    let (frames, mut rx) = feed.connect();
    let (tx, body_rx) = tokio::sync::mpsc::channel::<String>(64);
    let feed = feed.clone();
    tokio::spawn(async move {
        for f in frames {
            if tx.send(f).await.is_err() {
                return;
            }
        }
        loop {
            let next = match rx.recv().await {
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

async fn say(req: hyper::Request<hyper::body::Incoming>, sayer: &Sayer) -> Resp {
    let body = http_body_util::Limited::new(req.into_body(), 64 * 1024);
    let Ok(bytes) = body.collect().await.map(|c| c.to_bytes()) else {
        return text(413, "too much to say at once", "text/plain");
    };
    let Ok(say) = serde_json::from_slice::<Say>(&bytes) else {
        return text(400, "expected {\"pane\": …, \"text\": …}", "text/plain");
    };
    let sayer = sayer.clone();
    let sent = tokio::task::spawn_blocking(move || sayer(&say.pane, &say.text)).await;
    match sent {
        Ok(Ok(())) => text(204, "", "text/plain"),
        Ok(Err(e)) => text(502, format!("{e:#}"), "text/plain"),
        Err(_) => text(500, "the sender panicked", "text/plain"),
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
        let mut next_owner = 0;
        let roster: BTreeSet<String> = ["claude".to_string(), "codex".to_string()].into();
        let dir = tempfile::tempdir().unwrap();
        let file = |n: &str| dir.path().join(n);
        let mut wanted = Wanted::default();
        wanted.insert("claude".into(), ("s1".into(), file("s1.jsonl")));
        let with = retarget_tails(&wanted, &roster, &feed, &mut tails, &mut next_owner);
        assert_eq!(with, ["claude".to_string()].into());
        let owner1 = feed.snapshot().turns["claude"].owner;
        feed.turns_reset("claude", owner1, "s1", 1, vec![say("a")]);
        assert_eq!(feed.snapshot().turns["claude"].turns.len(), 1);

        let (_, mut rx) = feed.connect();
        wanted.insert("claude".into(), ("s2".into(), file("s2.jsonl")));
        retarget_tails(&wanted, &roster, &feed, &mut tails, &mut next_owner);
        let owner2 = feed.snapshot().turns["claude"].owner;
        assert_ne!(owner1, owner2);
        let mut page = feed::PageModel::default();
        page.apply(&rx.try_recv().unwrap());
        assert_eq!(page.agents["claude"].session, "s2");
        assert!(page.agents["claude"].turns.is_empty());
        feed.turns_reset("claude", owner1, "s1", 2, vec![say("stale")]);
        assert_eq!(feed.snapshot().turns["claude"].session, "s2");

        wanted.remove("claude");
        let with = retarget_tails(&wanted, &roster, &feed, &mut tails, &mut next_owner);
        assert!(with.is_empty() && tails.is_empty());
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

    /// Attached to a process that is gone, the server ends at once;
    /// attached to one that lives, or to none, it does not.
    #[tokio::test]
    async fn the_server_ends_with_the_process_it_is_attached_to() {
        use std::time::Duration;
        // A pid no process has: the largest macOS/Linux will hand out
        // is far below this, and a kill(pid, 0) on it is ESRCH.
        let gone = 4_000_000u32;
        assert!(!process_exists(gone));
        tokio::time::timeout(Duration::from_secs(1), attached_gone(Some(gone)))
            .await
            .expect("ends at once");
        let me = std::process::id();
        assert!(process_exists(me));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), attached_gone(Some(me)))
                .await
                .is_err(),
            "still attached"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), attached_gone(None))
                .await
                .is_err(),
            "attached to nothing, ends for nothing"
        );
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

    /// A clank identity server on `port`, answering for `repo`.
    fn identity_server(port: u16, repo: &Path) -> (tokio::task::JoinHandle<()>, Identity) {
        let identity = Identity {
            clank: "web".into(),
            repo: canonical(repo),
            session: "s".into(),
            pid: 777,
            port,
            attached_to: None,
        };
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let site = tempfile::tempdir().unwrap();
        let (repo, id) = (repo.to_path_buf(), identity.clone());
        let handle = tokio::spawn(async move {
            let _site = site;
            let sayer: Sayer = Arc::new(|_, _| Ok(()));
            let _ = serve(listener, Feed::new(4), sayer, "s", repo, PathBuf::new(), id).await;
        });
        (handle, identity)
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// The port policy, against real listeners: nothing remembered →
    /// one sampled and remembered; remembered and free → that one;
    /// `--port` → exactly that, nothing remembered; remembered but a
    /// stranger's → a fresh one, remembered in its place; remembered
    /// and this repo's own server → already on; another repo's → a
    /// fresh one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_repo_remembers_its_port_and_asks_who_holds_it() {
        let repo = repo_with_config();
        assert_eq!(crate::agent_store::web_port(repo.path()).unwrap(), None);
        let Listen::Bound { listener, port } = listen(repo.path(), None).await.unwrap() else {
            panic!("bound")
        };
        assert_eq!(
            crate::agent_store::web_port(repo.path()).unwrap(),
            Some(port)
        );
        drop(listener);
        let Listen::Bound { port: again, .. } = listen(repo.path(), None).await.unwrap() else {
            panic!("bound")
        };
        assert_eq!(again, port, "remembered, and free");

        let explicit = free_port();
        let Listen::Bound { port: told, .. } = listen(repo.path(), Some(explicit)).await.unwrap()
        else {
            panic!("bound")
        };
        assert_eq!(told, explicit);
        assert_eq!(
            crate::agent_store::web_port(repo.path()).unwrap(),
            Some(port),
            "--port remembers nothing"
        );

        // A stranger on the remembered port: not asked twice, replaced.
        let stranger = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let Listen::Bound { port: fresh, .. } = listen(repo.path(), None).await.unwrap() else {
            panic!("bound")
        };
        assert_ne!(fresh, port);
        assert_eq!(
            crate::agent_store::web_port(repo.path()).unwrap(),
            Some(fresh)
        );
        drop(stranger);

        // This repo's own server on the remembered port: already on.
        let (server, identity) = identity_server(fresh, repo.path());
        let Listen::AlreadyOn { identity: found } = listen(repo.path(), None).await.unwrap() else {
            panic!("already on")
        };
        assert_eq!(found, identity);
        assert_eq!(
            crate::agent_store::web_port(repo.path()).unwrap(),
            Some(fresh)
        );
        server.abort();

        // Another repo's server on it: theirs now; a fresh one.
        let other = repo_with_config();
        let (server, _) = identity_server(fresh, other.path());
        let Listen::Bound { port: moved, .. } = listen(repo.path(), None).await.unwrap() else {
            panic!("bound")
        };
        assert_ne!(moved, fresh);
        assert_eq!(
            crate::agent_store::web_port(repo.path()).unwrap(),
            Some(moved)
        );
        server.abort();
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
        let identity = Identity {
            clank: "web".into(),
            repo: canonical(repo.path()),
            session: "clank-test".into(),
            pid: std::process::id(),
            port,
            attached_to: Some(4242),
        };
        let server = tokio::spawn({
            let feed = feed.clone();
            let (repo, site) = (repo.path().to_path_buf(), site.path().to_path_buf());
            let identity = identity.clone();
            async move { serve(listener, feed, sayer, "clank-test", repo, site, identity).await }
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

        // Who is serving, as JSON, for whoever finds the port taken.
        let who: Identity = client
            .get(format!("{base}/identity"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(who, identity);
        assert_eq!(identity_at(port).await, Some(identity.clone()));

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

        server.abort();
    }
}
