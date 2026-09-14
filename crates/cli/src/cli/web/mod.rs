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

    let feed = Feed::new(64);
    let table = open_zellij::agent_pane_table(&panes, &repo);
    feed.panes(table.clone());
    let subscription = Arc::new(Mutex::new(start_subscription(&session, &table, &feed)));

    spawn_status_loop(repo.clone(), feed.clone());
    spawn_pane_poll(
        session.clone(),
        repo.clone(),
        feed.clone(),
        subscription.clone(),
    );

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port))
        .await
        .map_err(|e| anyhow::anyhow!("cannot listen on 127.0.0.1:{}: {e}", args.port))?;
    println!(
        "clank web: http://127.0.0.1:{}  (session {session}, {} agent pane{})",
        args.port,
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
    let served = tokio::select! {
        r = serve(listener, feed, sayer, &session) => r,
        _ = shutdown_signal() => Ok(()),
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
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Start (or restart) the one subscription child for `table`'s live
/// panes, and the thread that reads it into the feed. The previous
/// child, if any, is the caller's to drop — dropping kills it, and
/// its reader thread ends on EOF.
fn start_subscription(session: &str, table: &[PaneMeta], feed: &Feed) -> Option<SubscribeChild> {
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
                    transcript::Parsed::ToolOutput { id, output } => {
                        feed.tool_output(&agent, owner, &session, generation, &id, output)
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
                transcript::Parsed::ToolOutput { id, output } => {
                    if let Some(t) = turns.iter_mut().find(|t| t.id == id)
                        && let transcript::Body::Tool { output: slot, .. } = &mut t.body
                    {
                        *slot = Some(output);
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
                let fresh = start_subscription(&session, &table, &feed);
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
async fn serve(
    listener: tokio::net::TcpListener,
    feed: Feed,
    sayer: Sayer,
    session: &str,
) -> anyhow::Result<()> {
    let page: Arc<str> = Arc::from(PAGE.replace(
        "window.CLANK_SESSION || 'clank'",
        &format!("{}", serde_json::json!(session)),
    ));
    loop {
        let (stream, _) = listener.accept().await?;
        let io = hyper_util::rt::TokioIo::new(stream);
        let (feed, sayer, page) = (feed.clone(), sayer.clone(), page.clone());
        let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
            let (feed, sayer, page) = (feed.clone(), sayer.clone(), page.clone());
            async move { Ok::<_, std::convert::Infallible>(route(req, &feed, &sayer, &page).await) }
        });
        tokio::spawn(async move {
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, svc)
                .await;
        });
    }
}

async fn route(
    req: hyper::Request<hyper::body::Incoming>,
    feed: &Feed,
    sayer: &Sayer,
    page: &str,
) -> Resp {
    match (req.method().as_str(), req.uri().path()) {
        ("GET", "/") => text(200, page.to_string(), "text/html; charset=utf-8"),
        ("GET", "/events") => events(feed),
        ("POST", "/say") => say(req, sayer).await,
        _ => text(404, "not here", "text/plain"),
    }
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
        let server = tokio::spawn({
            let feed = feed.clone();
            async move { serve(listener, feed, sayer, "clank-test").await }
        });
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();

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
