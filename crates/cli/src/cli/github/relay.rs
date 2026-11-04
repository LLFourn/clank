//! The webhook transport: one `gh webhook forward` per upstream repo
//! per process, its loopback listener, and the bounded inbox each
//! subscribed watch reads from.
//!
//! GitHub lets one CLI forwarder hold a repo's webhook slot at a time,
//! so the forwarder belongs to the repo, not to a watch: a
//! [`RepoTransport`] subscribes to the union of what its watches need
//! and fans every delivery out to each of them. A delivery is RAW on
//! purpose — each watch classifies at drain time through its own
//! selection, own-actor filter, log and dedup — so nothing here decides
//! whether an event wakes anybody. This module is the transport; the
//! watch is the judge.
//!
//! A forwarder is LISTENING only once GitHub's `ping` for its freshly
//! created hook has come through our listener end to end; gh-webhook's
//! banner is printed before the hook is activated, so it proves
//! nothing.

use clank_core::agent_config::GithubEventKind;

/// One loopback request may not buffer unboundedly: the count-bounded
/// inbox doesn't bound MEMORY if a single body is huge (codex
/// 4d77b83). Real webhook payloads are kilobytes; a MiB is generous.
const MAX_WEBHOOK_BODY: usize = 1 << 20;

/// Extract one relay delivery from a webhook request: the event name
/// from `X-GitHub-Event`, the payload from the JSON body. Generic
/// over the body so tests drive it with fabricated requests.
pub(super) async fn webhook_from_request<B>(req: hyper::Request<B>) -> Option<RawDelivery>
where
    B: hyper::body::Body,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    use http_body_util::BodyExt;
    let name = req
        .headers()
        .get("x-github-event")?
        .to_str()
        .ok()?
        .to_string();
    let limited = http_body_util::Limited::new(req.into_body(), MAX_WEBHOOK_BODY);
    let bytes = limited.collect().await.ok()?.to_bytes();
    let payload = serde_json::from_slice(&bytes).ok()?;
    Some((name, payload))
}

/// Serve the loopback listener: each delivery goes to the transport,
/// 200 back to the forwarder. Connections are served INLINE — no
/// detached per-connection tasks, so dropping the forwarder's run
/// cancels everything it owns.
async fn run_relay_listener(listener: tokio::net::TcpListener, link: ForwarderLink) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let io = hyper_util::rt::TokioIo::new(stream);
        let link = link.clone();
        let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
            let link = link.clone();
            async move {
                if let Some(raw) = webhook_from_request(req).await {
                    link.deliver(raw);
                }
                Ok::<_, std::convert::Infallible>(hyper::Response::new(http_body_util::Empty::<
                    hyper::body::Bytes,
                >::new()))
            }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(io, svc)
            .await;
    }
}

/// The webhook event names a watch's sub-kinds need — what its share
/// of `gh webhook forward --events` is.
pub(super) fn webhook_event_names(events: &[GithubEventKind]) -> Vec<&'static str> {
    let mut names = Vec::new();
    for k in events {
        let name = match k {
            GithubEventKind::PrOpened | GithubEventKind::PrUpdated | GithubEventKind::PrMerged => {
                "pull_request"
            }
            GithubEventKind::PrComment => "pull_request_review", // + comment kinds below
            GithubEventKind::ReviewSubmitted => "pull_request_review",
            GithubEventKind::IssueOpened | GithubEventKind::IssueClosed => "issues",
            GithubEventKind::IssueComment => "issue_comment",
            GithubEventKind::BranchPush => "push",
            // A completed suite is only a TRIGGER to re-read the
            // PR's checks — one App's suite is not the PR's result —
            // so the noisy per-job `check_run` is not subscribed.
            GithubEventKind::CiPassed | GithubEventKind::CiFailed => "check_suite",
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    // pr_comment spans three webhook events.
    if events.contains(&GithubEventKind::PrComment) {
        for extra in ["pull_request_review_comment", "issue_comment"] {
            if !names.contains(&extra) {
                names.push(extra);
            }
        }
    }
    // With no timer, a cue is the only reason the checks are read
    // again: the older commit-status API reports outside check suites,
    // and a new head is a build that has not been seen yet.
    if events.iter().any(|k| k.is_ci()) {
        for cue in super::coordinator::CHECK_CUES
            .iter()
            .chain(&[super::coordinator::HEAD_CHANGE])
        {
            if !names.contains(cue) {
                names.push(cue);
            }
        }
    }
    names
}

/// The forwarder's argv — pure, pinned by test (codex 4d77b83).
pub(super) fn forwarder_argv(repo: &str, events: &str, port: u16) -> Vec<String> {
    vec![
        "webhook".into(),
        "forward".into(),
        format!("--repo={repo}"),
        format!("--events={events}"),
        format!("--url=http://127.0.0.1:{port}"),
    ]
}

/// The control delivery GitHub sends when a hook is created; `gh
/// webhook forward` creates a fresh hook on every start, so its arrival
/// at our listener is the end-to-end proof the hook is live.
pub(super) const PING: &str = "ping";

/// gh-webhook's warning when a delivery could not reach our listener.
/// It logs this and keeps running, so a living child is not a healthy
/// one. From `handleWebsocket` in
/// <https://github.com/cli/gh-webhook/blob/main/webhook/forward.go>.
pub(super) const FORWARDING_ERROR: &str = "warning: error forwarding event:";
/// GitHub's 422 body when the repo's CLI webhook slot is taken, which
/// gh-webhook's `createHook` prints wrapped in "error creating
/// webhook: ". From GitHub's API, via
/// <https://github.com/cli/gh-webhook/blob/main/webhook/create_webhook.go>.
pub(super) const HOOK_ALREADY_EXISTS: &str = "Hook already exists";
/// gh-webhook's `createHook` on a 403: the user can't create hooks.
pub(super) const NO_ACCESS: &str = "you do not have access to this feature";
/// gh's own answer when the `cli/gh-webhook` extension is absent.
pub(super) const NO_EXTENSION: &str = "unknown command \"webhook\"";

/// What one line of the forwarder's output says about its health, if
/// anything. Everything else (the banner included) is noise.
pub(super) fn classify_output_line(line: &str) -> Option<RelayFailure> {
    if line.contains(HOOK_ALREADY_EXISTS) {
        Some(RelayFailure::SlotConflict)
    } else if line.contains(NO_ACCESS) {
        Some(RelayFailure::NoAdmin)
    } else if line.contains(NO_EXTENSION) {
        Some(RelayFailure::NoExtension)
    } else if line.contains(FORWARDING_ERROR) {
        Some(RelayFailure::ForwardingError(bounded(line)))
    } else {
        None
    }
}

fn bounded(s: &str) -> String {
    s.trim().chars().take(200).collect()
}

/// How a forwarder run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ForwarderEnd {
    /// It never ran: no `gh`, or no listener to point it at.
    NotStarted(String),
    /// It ran and exited, with the bounded human reason.
    Exited(String),
}

/// What a forwarder run reports back while it runs: each delivery its
/// listener receives, and each line the child writes.
#[derive(Clone)]
pub(crate) struct ForwarderLink {
    deliver: std::sync::Arc<dyn Fn(RawDelivery) + Send + Sync>,
    lines: tokio::sync::mpsc::UnboundedSender<String>,
}

impl ForwarderLink {
    pub(crate) fn deliver(&self, raw: RawDelivery) {
        (self.deliver)(raw);
    }

    pub(crate) fn line(&self, line: &str) {
        let _ = self.lines.send(line.to_string());
    }
}

/// The forwarder process seam: run until terminal, reporting through
/// the link. Dropping the future must end the child. Injected so the
/// transport's lifecycle is scriptable without gh (codex 4d77b83).
pub(crate) trait Forwarder: Send + Sync + 'static {
    fn run(
        &self,
        repo: String,
        events: String,
        link: ForwarderLink,
    ) -> impl std::future::Future<Output = ForwarderEnd> + Send;
}

/// Production forwarder: a loopback-only listener (by construction —
/// bound to 127.0.0.1) and `gh webhook forward` in its own process
/// group pointed at it, both owned by the run.
pub(super) struct GhForwarder;

impl Forwarder for GhForwarder {
    async fn run(&self, repo: String, events: String, link: ForwarderLink) -> ForwarderEnd {
        use crate::proc_run::{Group, Out, Spec};
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
            Ok(l) => l,
            Err(e) => return ForwarderEnd::NotStarted(format!("loopback listener: {e}")),
        };
        let port = match listener.local_addr() {
            Ok(a) => a.port(),
            Err(e) => return ForwarderEnd::NotStarted(format!("loopback listener: {e}")),
        };
        let spec = Spec::new("gh")
            .args(forwarder_argv(&repo, &events, port))
            .stderr(Out::Piped)
            .group(Group::New);
        tokio::select! {
            () = run_relay_listener(listener, link.clone()) => {
                ForwarderEnd::Exited("the loopback listener stopped".into())
            }
            end = run_forwarder_process(spec, &link) => end,
        }
    }
}

/// The longest line passed on whole; a longer one is cut there so a
/// newline-free stream can't grow without bound.
const MAX_LINE: usize = 4096;

/// Supervise a spawned forwarder-shaped command to its end, handing
/// every stderr line to the link as it arrives. stderr is drained
/// CONCURRENTLY with the wait — a chatty extension that fills the OS
/// pipe would otherwise block before exit and the wait would never
/// complete (codex 24c3355); the post-exit drain uses the same absolute
/// deadline discipline as command sources.
async fn run_forwarder_process(spec: crate::proc_run::Spec, link: &ForwarderLink) -> ForwarderEnd {
    use tokio::io::AsyncReadExt;
    let mut child = match crate::proc_run::start(spec) {
        Ok(c) => c,
        Err(e) => return ForwarderEnd::NotStarted(format!("failed to start: {e}")),
    };
    let mut stderr = child.take_stderr_async();
    let mut tail = crate::cli::wait::RingTail::new(4096);
    let mut partial: Vec<u8> = Vec::new();
    let mut buf = [0u8; 1024];
    let take = |bytes: &[u8], partial: &mut Vec<u8>, tail: &mut crate::cli::wait::RingTail| {
        tail.push(bytes);
        for &b in bytes {
            if b == b'\n' || partial.len() >= MAX_LINE {
                link.line(&String::from_utf8_lossy(partial));
                partial.clear();
            }
            if b != b'\n' {
                partial.push(b);
            }
        }
    };
    let status = loop {
        tokio::select! {
            exited = child.exited_async() => {
                child.kill();
                break exited.and_then(|()| child.wait());
            }
            r = async { stderr.as_mut().unwrap().read(&mut buf).await }, if stderr.is_some() => {
                match r {
                    Ok(0) | Err(_) => stderr = None,
                    Ok(n) => take(&buf[..n], &mut partial, &mut tail),
                }
            }
        }
    };
    // Catch a final line written just before exit — bounded both ways.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(100), async {
        while let Some(pipe) = stderr.as_mut() {
            match tokio::time::timeout(std::time::Duration::from_millis(20), pipe.read(&mut buf))
                .await
            {
                Ok(Ok(n)) if n > 0 => take(&buf[..n], &mut partial, &mut tail),
                _ => break,
            }
        }
    })
    .await;
    if !partial.is_empty() {
        link.line(&String::from_utf8_lossy(&partial));
    }
    let text = tail.into_string();
    let last_line = text.lines().last().unwrap_or("").to_string();
    match status {
        Ok(st) => ForwarderEnd::Exited(bounded(&format!("exited {st}: {last_line}"))),
        Err(e) => ForwarderEnd::Exited(bounded(&format!("wait failed: {e}"))),
    }
}

/// Why a repo's forwarder is not listening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RelayFailure {
    /// `gh webhook forward` could not be run at all.
    NotStarted(String),
    NoExtension,
    NoAdmin,
    /// Somebody else — another monitor, another machine, a person's own
    /// `gh webhook forward` — holds the repo's webhook slot.
    SlotConflict,
    /// The child ran but GitHub's `ping` never reached the listener.
    NoPing,
    /// The child is alive but could not hand a delivery to us.
    ForwardingError(String),
    Exited(String),
}

impl RelayFailure {
    /// The reason and, where there is one, the fix.
    pub(crate) fn describe(&self, repo: &str) -> String {
        match self {
            RelayFailure::NotStarted(why) => format!(
                "`gh webhook forward` could not run ({why}); install gh from https://cli.github.com"
            ),
            RelayFailure::NoExtension => "the gh-webhook extension is missing; run \
                 `gh extension install cli/gh-webhook`"
                .to_string(),
            RelayFailure::NoAdmin => format!("needs admin on {repo} to create a webhook"),
            RelayFailure::SlotConflict => format!(
                "another forwarder holds the webhook slot for {repo} (another clank monitor, \
                 another machine, or a `gh webhook forward` of your own); stop it to free the slot"
            ),
            RelayFailure::NoPing => format!(
                "the forwarder started but GitHub's ping never arrived within {}s",
                READY_TIMEOUT.as_secs()
            ),
            RelayFailure::ForwardingError(line) => {
                format!("the forwarder could not deliver to the local listener ({line})")
            }
            RelayFailure::Exited(why) => format!("the forwarder {why}"),
        }
    }

    fn from_end(end: ForwarderEnd) -> Self {
        match end {
            ForwarderEnd::NotStarted(why) => RelayFailure::NotStarted(why),
            ForwarderEnd::Exited(why) => RelayFailure::Exited(why),
        }
    }
}

/// A transport that is not listening, and when it tries again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayDown {
    pub(crate) cause: RelayFailure,
    pub(crate) retry_in: std::time::Duration,
}

/// Where a repo's transport is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum RelayHealth {
    #[default]
    Starting,
    /// Proven ready: GitHub's ping came through.
    Listening,
    Down(RelayDown),
}

/// How long a started forwarder has to prove itself with `ping`.
pub(super) const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const RETRY_BASE: std::time::Duration = std::time::Duration::from_secs(5);
const RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(300);
/// A held slot is somebody else's to release, which takes a person,
/// not a moment: retrying it in seconds would only add churn.
pub(super) const CONFLICT_RETRY_BASE: std::time::Duration = std::time::Duration::from_secs(120);
const CONFLICT_RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(1800);

fn backoff(cause: &RelayFailure, failures: u32) -> std::time::Duration {
    let (base, cap) = match cause {
        RelayFailure::SlotConflict => (CONFLICT_RETRY_BASE, CONFLICT_RETRY_CAP),
        _ => (RETRY_BASE, RETRY_CAP),
    };
    base.saturating_mul(1 << failures.saturating_sub(1).min(16))
        .min(cap)
}

/// One relay delivery as the loopback listener hands it over: the
/// webhook event NAME (the `X-GitHub-Event` header) plus the payload.
/// RAW on purpose — classification happens at DRAIN time in the watch
/// loop, where `own_login` is resolved, so a delivery arriving before
/// identity resolution can never bypass the fail-closed own-actor
/// filter (codex c29a07c): it simply waits in the inbox.
pub(crate) type RawDelivery = (String, serde_json::Value);

/// Ingress bound of one watch's inbox. Bounded so a stalled watch
/// cannot grow memory; what it evicts becomes a reconcile the watch
/// owes, never a silent loss.
pub(super) const RELAY_BUFFER_CAP: usize = 256;

/// What a watch hears from its transport. Health, catch-up debt and
/// check cues are CONTROL state, held apart from the delivery queue:
/// none of them can be evicted by a burst of actions or held behind the
/// own-action filter, and with no timer to repair them none may be
/// lost.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Heard {
    /// The transport is listening now.
    Listening,
    /// The transport is down now; the watch says so, loudly.
    Down(RelayDown),
    /// A start of listening happened, so the watch owes a catch-up. It
    /// says nothing about whether the transport is STILL listening:
    /// that is [`Heard::Listening`] / [`Heard::Down`], latest only.
    CatchUp,
    /// The checks may have moved: owe a checks read.
    ChecksCue,
    /// This many deliveries were evicted at ingress: the watch owes a
    /// reconcile.
    Overflowed(u64),
    Delivery(RawDelivery),
}

/// One watch's inbox: a BOUNDED drop-oldest queue whose capacity lives
/// at PRODUCER ingress (codex 840725c) — `push` itself evicts, so no
/// pending await anywhere in the consumer (a hung /user call, a slow
/// poll) can let deliveries accumulate beyond the cap — plus the
/// control state that must never be evicted with them. std Mutex
/// (no await while held) + Notify for the consumer wake.
#[derive(Default)]
pub(crate) struct RelayInbox {
    state: std::sync::Mutex<InboxState>,
    notify: tokio::sync::Notify,
}

#[derive(Default)]
struct InboxState {
    queue: std::collections::VecDeque<RawDelivery>,
    dropped: u64,
    /// Latest health not yet reported; a newer one replaces it, so a
    /// stale start can never be reported after a failure.
    health: Option<RelayHealth>,
    catch_up: bool,
    checks_cue: bool,
}

impl RelayInbox {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Take one delivery: a check cue sets the cue and is no action; a
    /// PR head change is both; anything else is queued, evicting the
    /// oldest at capacity.
    pub(crate) fn push(&self, raw: RawDelivery) {
        {
            let mut s = self.state.lock().unwrap();
            let check = super::coordinator::CHECK_CUES.contains(&raw.0.as_str());
            if check || super::coordinator::is_head_change(&raw.0, &raw.1) {
                s.checks_cue = true;
            }
            if !check {
                if s.queue.len() == RELAY_BUFFER_CAP {
                    s.queue.pop_front();
                    s.dropped += 1;
                }
                s.queue.push_back(raw);
            }
        }
        self.notify.notify_one();
    }

    pub(crate) fn started(&self) {
        {
            let mut s = self.state.lock().unwrap();
            s.catch_up = true;
            s.health = Some(RelayHealth::Listening);
        }
        self.notify.notify_one();
    }

    pub(crate) fn down(&self, down: RelayDown) {
        {
            self.state.lock().unwrap().health = Some(RelayHealth::Down(down));
        }
        self.notify.notify_one();
    }

    /// The next thing to act on. Deliveries are held back while
    /// `deliveries` is false — they wait at ingress, bounded — but the
    /// control state still comes through. Cancellation-safe: the
    /// check-then-wait loop re-checks after every wake, and
    /// `notify_one` stores a permit when nobody is waiting.
    pub(crate) async fn next(&self, deliveries: bool) -> Heard {
        loop {
            {
                let mut s = self.state.lock().unwrap();
                match s.health.take() {
                    Some(RelayHealth::Listening) => return Heard::Listening,
                    Some(RelayHealth::Down(down)) => return Heard::Down(down),
                    Some(RelayHealth::Starting) | None => {}
                }
                if std::mem::take(&mut s.catch_up) {
                    return Heard::CatchUp;
                }
                if std::mem::take(&mut s.checks_cue) {
                    return Heard::ChecksCue;
                }
                if s.dropped > 0 {
                    return Heard::Overflowed(std::mem::take(&mut s.dropped));
                }
                if deliveries && let Some(raw) = s.queue.pop_front() {
                    return Heard::Delivery(raw);
                }
            }
            self.notify.notified().await;
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.state.lock().unwrap().queue.len()
    }
}

type Registry = std::sync::Arc<
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<RepoTransport>>>,
>;

/// Every repo's transport in this process. A monitor is one process
/// serving one repo, so this is the per-monitor scope: two monitors
/// watching the same upstream each run a forwarder, and the second
/// meets [`RelayFailure::SlotConflict`].
pub(crate) struct Transports<Fw: Forwarder> {
    forwarder: std::sync::Arc<Fw>,
    repos: Registry,
}

/// The process's transports over the real `gh webhook forward`.
pub(super) fn shared_transports() -> &'static Transports<GhForwarder> {
    static TRANSPORTS: std::sync::OnceLock<Transports<GhForwarder>> = std::sync::OnceLock::new();
    TRANSPORTS.get_or_init(|| Transports::new(GhForwarder))
}

impl<Fw: Forwarder> Transports<Fw> {
    pub(crate) fn new(forwarder: Fw) -> Self {
        Self {
            forwarder: std::sync::Arc::new(forwarder),
            repos: Default::default(),
        }
    }

    /// Subscribe one watch to `repo`'s deliveries named in `names`.
    ///
    /// Names the running forwarder already covers join at once — with
    /// a [`Heard::CatchUp`] straight away if it is already listening.
    /// Any others widen the subscription, which restarts the forwarder
    /// with the union; that restart is a new start of listening for
    /// every subscriber. Each start's catch-up is bounded by how far
    /// the feed lags, the same limit as any other start. Must be called
    /// inside a tokio runtime.
    pub(crate) fn subscribe(&self, repo: &str, names: &[&'static str]) -> Subscription {
        let inbox = std::sync::Arc::new(RelayInbox::new());
        let mut repos = self.repos.lock().unwrap();
        let transport = repos
            .entry(repo.to_string())
            .or_insert_with(|| {
                std::sync::Arc::new(RepoTransport {
                    repo: repo.to_string(),
                    state: Default::default(),
                    wake: tokio::sync::Notify::new(),
                })
            })
            .clone();
        let id = {
            let mut s = transport.state.lock().unwrap();
            let id = s.next_id;
            s.next_id += 1;
            let names: std::collections::BTreeSet<&'static str> = names.iter().copied().collect();
            let covered = names.is_subset(&s.wanted);
            s.wanted.extend(names.iter().copied());
            s.subscribers.push(Subscriber {
                id,
                names,
                inbox: inbox.clone(),
            });
            if covered {
                match &s.health {
                    RelayHealth::Listening => inbox.started(),
                    RelayHealth::Down(down) => inbox.down(down.clone()),
                    RelayHealth::Starting => {}
                }
            }
            if !covered {
                s.health = RelayHealth::Starting;
            }
            if s.driver.is_none() {
                s.driver = Some(tokio::spawn(drive(
                    transport.clone(),
                    self.forwarder.clone(),
                )));
            } else if !covered {
                transport.wake.notify_one();
            }
            id
        };
        drop(repos);
        Subscription {
            inbox,
            link: Some((self.repos.clone(), transport, id)),
        }
    }
}

/// One watch's place on its repo's transport. Dropping it unsubscribes;
/// the last one to go stops the forwarder and frees the slot.
pub(crate) struct Subscription {
    pub(crate) inbox: std::sync::Arc<RelayInbox>,
    link: Option<(Registry, std::sync::Arc<RepoTransport>, u64)>,
}

impl Subscription {
    /// An inbox on no transport, for tests that drive it by hand.
    #[cfg(test)]
    pub(crate) fn detached(inbox: std::sync::Arc<RelayInbox>) -> Self {
        Self { inbox, link: None }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let Some((repos, transport, id)) = self.link.take() else {
            return;
        };
        let mut repos = repos.lock().unwrap();
        let mut s = transport.state.lock().unwrap();
        s.subscribers.retain(|sub| sub.id != id);
        // The union is not narrowed while others remain: narrowing
        // would restart the forwarder and cost every other watch a
        // catch-up for nothing.
        if s.subscribers.is_empty() {
            if let Some(driver) = s.driver.take() {
                driver.abort();
            }
            drop(s);
            if repos
                .get(&transport.repo)
                .is_some_and(|t| std::sync::Arc::ptr_eq(t, &transport))
            {
                repos.remove(&transport.repo);
            }
        }
    }
}

/// One upstream repo's webhook transport, shared by every watch of it
/// in this process.
struct RepoTransport {
    repo: String,
    state: std::sync::Mutex<TransportState>,
    /// The driver's wake: a ping arrived, or the subscription widened.
    wake: tokio::sync::Notify,
}

#[derive(Default)]
struct TransportState {
    subscribers: Vec<Subscriber>,
    next_id: u64,
    /// The union of every subscriber's names since the transport
    /// started; only ever widened.
    wanted: std::collections::BTreeSet<&'static str>,
    /// Which forwarder run is current, so a ping is credited only to
    /// the hook it was sent for.
    generation: u64,
    pinged: bool,
    health: RelayHealth,
    driver: Option<tokio::task::JoinHandle<()>>,
}

struct Subscriber {
    id: u64,
    names: std::collections::BTreeSet<&'static str>,
    inbox: std::sync::Arc<RelayInbox>,
}

impl RepoTransport {
    /// A delivery from run `generation`'s listener. Every delivery is
    /// live — the hook was created after the subscription began — so
    /// each goes to every watch that subscribed to its name, whether or
    /// not the run has proven itself yet (codex b4f6fad).
    fn receive(&self, generation: u64, raw: RawDelivery) {
        let mut s = self.state.lock().unwrap();
        if raw.0 == PING {
            if generation == s.generation {
                s.pinged = true;
                drop(s);
                self.wake.notify_one();
            }
            return;
        }
        for sub in &s.subscribers {
            if sub.names.contains(raw.0.as_str()) {
                sub.inbox.push(raw.clone());
            }
        }
    }

    fn set_health(&self, health: RelayHealth) {
        let mut s = self.state.lock().unwrap();
        for sub in &s.subscribers {
            match &health {
                RelayHealth::Listening => sub.inbox.started(),
                RelayHealth::Down(down) => sub.inbox.down(down.clone()),
                RelayHealth::Starting => {}
            }
        }
        s.health = health;
    }
}

/// The transport's lifecycle: start the forwarder on the current union,
/// wait for its ping, then run until it fails or the union widens.
/// Every failure backs off ONCE for the whole transport, however many
/// watches share it, so competing watches cannot cause a restart loop.
async fn drive<Fw: Forwarder>(
    transport: std::sync::Arc<RepoTransport>,
    forwarder: std::sync::Arc<Fw>,
) {
    let mut failures = 0u32;
    loop {
        let (generation, running) = {
            let mut s = transport.state.lock().unwrap();
            s.generation += 1;
            s.pinged = false;
            (s.generation, s.wanted.clone())
        };
        let (lines, mut heard_lines) = tokio::sync::mpsc::unbounded_channel();
        let receiver = transport.clone();
        let link = ForwarderLink {
            deliver: std::sync::Arc::new(move |raw| receiver.receive(generation, raw)),
            lines,
        };
        let events = running.iter().copied().collect::<Vec<_>>().join(",");
        let failure = {
            let run = forwarder.run(transport.repo.clone(), events, link);
            tokio::pin!(run);
            let ready_by = tokio::time::Instant::now() + READY_TIMEOUT;
            let mut ready = false;
            loop {
                tokio::select! {
                    end = &mut run => {
                        let mut said = None;
                        while let Ok(line) = heard_lines.try_recv() {
                            said = said.or_else(|| classify_output_line(&line));
                        }
                        break Some(said.unwrap_or_else(|| RelayFailure::from_end(end)));
                    }
                    Some(line) = heard_lines.recv() => {
                        if let Some(failure) = classify_output_line(&line) {
                            break Some(failure);
                        }
                    }
                    () = tokio::time::sleep_until(ready_by), if !ready => {
                        break Some(RelayFailure::NoPing);
                    }
                    () = transport.wake.notified() => {
                        let (widened, pinged) = {
                            let s = transport.state.lock().unwrap();
                            (s.wanted != running, s.pinged)
                        };
                        if widened {
                            break None;
                        }
                        if pinged && !ready {
                            ready = true;
                            failures = 0;
                            transport.set_health(RelayHealth::Listening);
                        }
                    }
                }
            }
        };
        let Some(cause) = failure else {
            transport.set_health(RelayHealth::Starting);
            continue;
        };
        failures += 1;
        let retry_in = backoff(&cause, failures);
        transport.set_health(RelayHealth::Down(RelayDown { cause, retry_in }));
        tokio::time::sleep(retry_in).await;
    }
}

#[cfg(test)]
pub(in crate::cli::github) mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    /// How one scripted forwarder run behaves once started.
    #[derive(Clone)]
    pub(in crate::cli::github) enum Run {
        /// GitHub's ping arrives; the child stays up until ended.
        Ping,
        /// The banner and nothing else: never proven ready.
        BannerOnly,
        /// What gh-webhook prints when the slot is taken, then exit.
        SlotTaken,
        NotStarted(&'static str),
    }

    /// A scripted `gh webhook forward`: each run takes the next
    /// behaviour (then [`Run::Ping`] forever), records the events it was
    /// started with, and counts itself live until its future drops —
    /// which is how the real child dies.
    #[derive(Clone, Default)]
    pub(in crate::cli::github) struct FakeForwarder {
        script: std::sync::Arc<Mutex<std::collections::VecDeque<Run>>>,
        pub(in crate::cli::github) runs: std::sync::Arc<Mutex<Vec<String>>>,
        pub(in crate::cli::github) live: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        links: std::sync::Arc<Mutex<std::collections::HashMap<String, ForwarderLink>>>,
        ends: std::sync::Arc<
            Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Notify>>>,
        >,
    }

    struct Alive(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for Alive {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl FakeForwarder {
        pub(in crate::cli::github) fn scripted(runs: &[Run]) -> Self {
            let fake = Self::default();
            fake.script.lock().unwrap().extend(runs.iter().cloned());
            fake
        }

        pub(in crate::cli::github) fn started(&self) -> usize {
            self.runs.lock().unwrap().len()
        }

        pub(in crate::cli::github) fn alive(&self) -> usize {
            self.live.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn link(&self) -> ForwarderLink {
            self.links
                .lock()
                .unwrap()
                .get("o/r")
                .cloned()
                .expect("a run of o/r is up")
        }

        /// A delivery through the current o/r run's listener.
        pub(in crate::cli::github) fn deliver(&self, name: &str, payload: serde_json::Value) {
            self.link().deliver((name.to_string(), payload));
        }

        /// The current o/r child exits on its own.
        pub(in crate::cli::github) fn exit(&self) {
            self.ends
                .lock()
                .unwrap()
                .get("o/r")
                .expect("a run of o/r is up")
                .notify_one();
        }

        /// A line on the current o/r run's stderr.
        pub(in crate::cli::github) fn say(&self, line: &str) {
            self.link().line(line);
        }
    }

    impl Forwarder for FakeForwarder {
        async fn run(&self, repo: String, events: String, link: ForwarderLink) -> ForwarderEnd {
            let behaviour = self.script.lock().unwrap().pop_front().unwrap_or(Run::Ping);
            self.runs.lock().unwrap().push(events);
            self.live.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _alive = Alive(self.live.clone());
            let end = std::sync::Arc::new(tokio::sync::Notify::new());
            self.ends.lock().unwrap().insert(repo.clone(), end.clone());
            self.links.lock().unwrap().insert(repo, link.clone());
            link.line("Forwarding Webhook events from GitHub...");
            match behaviour {
                Run::Ping => link.deliver((
                    PING.into(),
                    serde_json::json!({"zen": "Keep it logically awesome."}),
                )),
                Run::BannerOnly => {}
                Run::SlotTaken => {
                    link.line(
                        "error creating webhook: HTTP 422: Validation Failed \
                         (https://api.github.com/repos/o/r/hooks)",
                    );
                    link.line("Hook already exists on this repository");
                    return ForwarderEnd::Exited("exited exit status: 1".into());
                }
                Run::NotStarted(why) => return ForwarderEnd::NotStarted(why.into()),
            }
            end.notified().await;
            ForwarderEnd::Exited(
                "exited exit status: 1: unable to connect to webhooks server, forwarding stopped"
                    .into(),
            )
        }
    }

    /// A subscription whose transport has already proven itself, for
    /// tests of what a watch does once listening.
    pub(in crate::cli::github) fn listening() -> Subscription {
        let inbox = std::sync::Arc::new(RelayInbox::new());
        inbox.started();
        Subscription::detached(inbox)
    }

    /// Let spawned tasks run to their next wait without moving time.
    pub(in crate::cli::github) async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    /// Everything the inbox holds right now, without waiting.
    pub(in crate::cli::github) async fn heard_now(inbox: &RelayInbox) -> Vec<Heard> {
        let mut out = Vec::new();
        while let Ok(h) = tokio::time::timeout(Duration::ZERO, inbox.next(true)).await {
            out.push(h);
        }
        out
    }

    #[tokio::test]
    async fn webhook_requests_map_to_raw_deliveries() {
        // The listener's extraction: X-GitHub-Event + JSON body →
        // RawDelivery; junk degrades to None (dropped, never a panic).
        use http_body_util::Full;
        use hyper::body::Bytes;
        let req = hyper::Request::builder()
            .header("X-GitHub-Event", "push")
            .body(Full::new(Bytes::from(r#"{"ref":"refs/heads/main"}"#)))
            .unwrap();
        let (name, payload) = webhook_from_request(req).await.expect("maps");
        assert_eq!(name, "push");
        assert_eq!(payload["ref"], "refs/heads/main");
        let req = hyper::Request::builder()
            .body(Full::new(Bytes::from("{}")))
            .unwrap();
        assert!(webhook_from_request(req).await.is_none());
        let req = hyper::Request::builder()
            .header("X-GitHub-Event", "push")
            .body(Full::new(Bytes::from("not json")))
            .unwrap();
        assert!(webhook_from_request(req).await.is_none());
    }

    #[test]
    fn webhook_event_names_cover_the_subscribed_kinds() {
        use GithubEventKind::*;
        let names = webhook_event_names(&[PrOpened, PrComment, BranchPush]);
        for expect in [
            "pull_request",
            "pull_request_review",
            "pull_request_review_comment",
            "issue_comment",
            "push",
        ] {
            assert!(names.contains(&expect), "{expect} missing: {names:?}");
        }
        assert_eq!(
            webhook_event_names(&[IssueOpened, IssueClosed]),
            vec!["issues"],
            "no over-subscription"
        );
    }

    #[test]
    fn forwarder_argv_is_pinned() {
        assert_eq!(
            forwarder_argv("o/r", "push,issues", 4242),
            vec![
                "webhook",
                "forward",
                "--repo=o/r",
                "--events=push,issues",
                "--url=http://127.0.0.1:4242",
            ]
        );
    }

    #[tokio::test]
    async fn oversized_webhook_bodies_are_rejected() {
        // codex 4d77b83: a count-bounded inbox doesn't bound memory if
        // one request is huge — the body collect is byte-limited.
        use http_body_util::Full;
        use hyper::body::Bytes;
        let big = vec![b'x'; MAX_WEBHOOK_BODY + 1];
        let req = hyper::Request::builder()
            .header("X-GitHub-Event", "push")
            .body(Full::new(Bytes::from(big)))
            .unwrap();
        assert!(webhook_from_request(req).await.is_none());
    }

    #[tokio::test]
    async fn forwarder_lines_survive_a_pipe_filling_child() {
        // codex 24c3355: a child that writes beyond the OS pipe
        // capacity before exiting must still reach its end — the
        // concurrent read keeps the pipe drained — and every line it
        // wrote reaches the link. 256 KiB of stderr ≫ any pipe buffer.
        let cmd = crate::proc_run::Spec::new("sh")
            .args([
                "-c",
                "i=0; while [ $i -lt 4096 ]; do printf \
                 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n' >&2; \
                 i=$((i+1)); done; echo the-final-line >&2; exit 3",
            ])
            .stderr(crate::proc_run::Out::Piped)
            .group(crate::proc_run::Group::New);
        let (lines, mut heard) = tokio::sync::mpsc::unbounded_channel();
        let link = ForwarderLink {
            deliver: std::sync::Arc::new(|_| {}),
            lines,
        };
        let end = tokio::time::timeout(Duration::from_secs(30), run_forwarder_process(cmd, &link))
            .await
            .expect("no deadlock on a full pipe");
        let ForwarderEnd::Exited(reason) = end else {
            panic!("it ran: {end:?}");
        };
        assert!(reason.contains("the-final-line"), "{reason}");
        let mut count = 0;
        let mut last = String::new();
        while let Ok(line) = heard.try_recv() {
            count += 1;
            last = line;
        }
        assert_eq!(count, 4097, "every line, split where the child split it");
        assert_eq!(last, "the-final-line");
    }

    #[test]
    fn the_ci_kinds_subscribe_to_their_cues() {
        // With no timer the cues are the only reason checks are read
        // again: both check surfaces and the head moving.
        let names = webhook_event_names(&[GithubEventKind::CiPassed]);
        assert_eq!(names, vec!["check_suite", "status", "pull_request"]);
        assert_eq!(
            webhook_event_names(&[GithubEventKind::CiFailed, GithubEventKind::CiPassed]),
            vec!["check_suite", "status", "pull_request"],
            "both kinds, one subscription each",
        );
        assert!(
            !webhook_event_names(&[GithubEventKind::BranchPush]).contains(&"status"),
            "a non-CI watch does not subscribe to statuses",
        );
    }

    /// The strings gh-webhook (and GitHub, through it) actually print,
    /// pinned: a drifted constant would leave a live-but-broken child
    /// looking healthy, or a held slot looking like missing admin.
    /// Sourced from cli/gh-webhook's webhook/forward.go and
    /// webhook/create_webhook.go (main, fetched 2026-11-04).
    #[test]
    fn the_gh_webhook_strings_are_pinned() {
        assert_eq!(FORWARDING_ERROR, "warning: error forwarding event:");
        assert_eq!(HOOK_ALREADY_EXISTS, "Hook already exists");
        assert_eq!(NO_ACCESS, "you do not have access to this feature");
        assert_eq!(PING, "ping");
        assert_eq!(
            classify_output_line(
                "warning: error forwarding event: Post \"http://127.0.0.1:4242\": dial tcp \
                 127.0.0.1:4242: connect: connection refused"
            ),
            Some(RelayFailure::ForwardingError(
                "warning: error forwarding event: Post \"http://127.0.0.1:4242\": dial tcp \
                 127.0.0.1:4242: connect: connection refused"
                    .into()
            )),
        );
        assert_eq!(
            classify_output_line("Hook already exists on this repository"),
            Some(RelayFailure::SlotConflict),
        );
        assert_eq!(
            classify_output_line("you do not have access to this feature"),
            Some(RelayFailure::NoAdmin),
        );
        assert_eq!(
            classify_output_line("unknown command \"webhook\" for \"gh\""),
            Some(RelayFailure::NoExtension),
        );
        assert_eq!(
            classify_output_line("Forwarding Webhook events from GitHub..."),
            None,
            "the banner proves nothing either way",
        );
        assert!(
            RelayFailure::NoAdmin
                .describe("o/r")
                .contains("needs admin on o/r to create a webhook")
        );
        let conflict = RelayFailure::SlotConflict.describe("o/r");
        assert!(
            conflict.contains("another forwarder holds the webhook slot for o/r")
                && !conflict.contains("admin"),
            "{conflict}"
        );
    }

    #[tokio::test]
    async fn an_overflowing_inbox_says_how_much_it_evicted() {
        let inbox = RelayInbox::new();
        for i in 0..RELAY_BUFFER_CAP + 3 {
            inbox.push(("issues".into(), serde_json::json!({ "i": i })));
        }
        let heard = heard_now(&inbox).await;
        assert_eq!(heard[0], Heard::Overflowed(3));
        assert_eq!(heard.len(), RELAY_BUFFER_CAP + 1);
        assert_eq!(
            heard[1],
            Heard::Delivery(("issues".into(), serde_json::json!({ "i": 3 }))),
            "the oldest went"
        );
    }

    fn down(cause: RelayFailure) -> RelayDown {
        RelayDown {
            cause,
            retry_in: Duration::from_secs(120),
        }
    }

    /// A start the consumer has not seen yet, then a failure: the
    /// current health is the failure, and the start still owes its
    /// catch-up — but never reports itself as listening.
    #[tokio::test]
    async fn a_start_then_a_failure_reports_the_failure_and_keeps_the_debt() {
        let inbox = RelayInbox::new();
        inbox.started();
        inbox.down(down(RelayFailure::SlotConflict));
        assert_eq!(
            heard_now(&inbox).await,
            vec![
                Heard::Down(down(RelayFailure::SlotConflict)),
                Heard::CatchUp
            ]
        );
    }

    #[tokio::test]
    async fn a_failure_then_a_start_reports_listening() {
        let inbox = RelayInbox::new();
        inbox.down(down(RelayFailure::NoPing));
        inbox.started();
        assert_eq!(
            heard_now(&inbox).await,
            vec![Heard::Listening, Heard::CatchUp]
        );
    }

    /// A check cue is control state: no burst of actions evicts it,
    /// and a head change is both a cue and an action.
    #[tokio::test]
    async fn a_check_cue_survives_any_burst_of_actions() {
        let inbox = RelayInbox::new();
        inbox.push(("status".into(), serde_json::json!({"state": "failure"})));
        for i in 0..RELAY_BUFFER_CAP + 5 {
            inbox.push(("issues".into(), serde_json::json!({ "i": i })));
        }
        let heard = heard_now(&inbox).await;
        assert_eq!(heard[0], Heard::ChecksCue);
        assert_eq!(heard[1], Heard::Overflowed(5));
        assert!(
            !heard
                .iter()
                .any(|h| matches!(h, Heard::Delivery((name, _)) if name == "status")),
            "a cue is not an action"
        );

        let head = (
            "pull_request".to_string(),
            serde_json::json!({"action": "synchronize"}),
        );
        inbox.push(head.clone());
        assert_eq!(
            heard_now(&inbox).await,
            vec![Heard::ChecksCue, Heard::Delivery(head)]
        );
    }

    #[tokio::test]
    async fn deliveries_wait_while_held_but_signals_do_not() {
        let inbox = RelayInbox::new();
        inbox.push(("issues".into(), serde_json::json!({})));
        inbox.started();
        assert_eq!(inbox.next(false).await, Heard::Listening);
        assert_eq!(inbox.next(false).await, Heard::CatchUp);
        assert!(
            tokio::time::timeout(Duration::ZERO, inbox.next(false))
                .await
                .is_err()
        );
        assert_eq!(inbox.len(), 1);
    }

    fn started_signals(heard: &[Heard]) -> usize {
        heard.iter().filter(|h| **h == Heard::CatchUp).count()
    }

    #[tokio::test(start_paused = true)]
    async fn watches_of_one_repo_share_one_forwarder_and_hear_only_their_names() {
        let fake = FakeForwarder::default();
        let transports = Transports::new(fake.clone());
        let a = transports.subscribe("o/r", &["issues"]);
        let b = transports.subscribe("o/r", &["issues"]);
        let other = transports.subscribe("elsewhere/r", &["issues"]);
        settle().await;
        assert_eq!(fake.started(), 2, "one per repo, not one per watch");
        assert_eq!(started_signals(&heard_now(&a.inbox).await), 1);
        assert_eq!(started_signals(&heard_now(&b.inbox).await), 1);
        drop(other);

        fake.deliver("issues", serde_json::json!({"n": 1}));
        fake.deliver("push", serde_json::json!({"n": 2}));
        for sub in [&a, &b] {
            assert_eq!(
                heard_now(&sub.inbox).await,
                vec![Heard::Delivery((
                    "issues".into(),
                    serde_json::json!({"n": 1})
                ))],
                "each subscriber hears its names, and a ping is never a delivery",
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn leaving_keeps_the_forwarder_for_the_rest_and_the_last_stops_it() {
        let fake = FakeForwarder::default();
        let transports = Transports::new(fake.clone());
        let a = transports.subscribe("o/r", &["issues"]);
        settle().await;
        let b = transports.subscribe("o/r", &["issues", "issue_comment"]);
        settle().await;
        assert_eq!(fake.started(), 2, "b widened the union");
        heard_now(&a.inbox).await;
        heard_now(&b.inbox).await;

        drop(b);
        settle().await;
        fake.deliver("issues", serde_json::json!({}));
        assert_eq!(heard_now(&a.inbox).await.len(), 1, "a still hears");
        assert_eq!(
            fake.started(),
            2,
            "leaving narrows nothing and restarts nothing"
        );
        assert_eq!(fake.alive(), 1);

        drop(a);
        settle().await;
        assert_eq!(fake.alive(), 0, "the last subscriber stops the child");
        let again = transports.subscribe("o/r", &["issues"]);
        settle().await;
        assert_eq!(fake.started(), 3, "and frees the slot for a fresh start");
        assert_eq!(started_signals(&heard_now(&again.inbox).await), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_covered_joiner_starts_listening_at_once() {
        let fake = FakeForwarder::default();
        let transports = Transports::new(fake.clone());
        let a = transports.subscribe("o/r", &["issues", "issue_comment"]);
        settle().await;
        heard_now(&a.inbox).await;
        let b = transports.subscribe("o/r", &["issues"]);
        assert_eq!(
            heard_now(&b.inbox).await,
            vec![Heard::Listening, Heard::CatchUp],
            "a joiner the running set covers gets its own start straight away",
        );
        settle().await;
        assert_eq!(fake.started(), 1);
        assert!(
            heard_now(&a.inbox).await.is_empty(),
            "and costs the others nothing"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_joiner_needing_more_restarts_the_forwarder_on_the_union() {
        use GithubEventKind::*;
        let fake = FakeForwarder::default();
        let transports = Transports::new(fake.clone());
        let issues = transports.subscribe("o/r", &webhook_event_names(&[IssueOpened]));
        settle().await;
        assert_eq!(started_signals(&heard_now(&issues.inbox).await), 1);

        let ci = transports.subscribe("o/r", &webhook_event_names(&[CiFailed]));
        settle().await;
        let runs = fake.runs.lock().unwrap().clone();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[1], "check_suite,issues,pull_request,status");
        assert_eq!(fake.alive(), 1, "the old child went first");
        assert_eq!(
            started_signals(&heard_now(&issues.inbox).await),
            1,
            "the restart is a new start for the watch that was already there"
        );
        assert_eq!(started_signals(&heard_now(&ci.inbox).await), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_slot_is_a_conflict_retried_once_per_transport() {
        let fake = FakeForwarder::scripted(&[Run::SlotTaken, Run::SlotTaken, Run::SlotTaken]);
        let transports = Transports::new(fake.clone());
        let a = transports.subscribe("o/r", &["issues"]);
        let b = transports.subscribe("o/r", &["issues"]);
        settle().await;
        for sub in [&a, &b] {
            let heard = heard_now(&sub.inbox).await;
            assert_eq!(
                heard,
                vec![Heard::Down(RelayDown {
                    cause: RelayFailure::SlotConflict,
                    retry_in: CONFLICT_RETRY_BASE,
                })],
                "never ready, and named as a conflict"
            );
        }
        tokio::time::sleep(CONFLICT_RETRY_BASE - Duration::from_secs(1)).await;
        assert_eq!(fake.started(), 1, "two watches, still one attempt");
        tokio::time::sleep(Duration::from_secs(2)).await;
        settle().await;
        assert_eq!(fake.started(), 2, "one retry for the transport");
        let heard = heard_now(&a.inbox).await;
        assert_eq!(heard.len(), 1);
        assert!(
            matches!(&heard[0], Heard::Down(d) if d.retry_in > CONFLICT_RETRY_BASE),
            "the ceiling grows: {heard:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_banner_without_a_ping_is_a_failed_start() {
        let fake = FakeForwarder::scripted(&[Run::BannerOnly]);
        let transports = Transports::new(fake.clone());
        let a = transports.subscribe("o/r", &["issues"]);
        settle().await;
        assert!(
            heard_now(&a.inbox).await.is_empty(),
            "the banner is not readiness"
        );
        tokio::time::sleep(READY_TIMEOUT + Duration::from_millis(1)).await;
        settle().await;
        assert_eq!(
            heard_now(&a.inbox).await,
            vec![Heard::Down(RelayDown {
                cause: RelayFailure::NoPing,
                retry_in: RETRY_BASE,
            })]
        );
        assert_eq!(
            fake.alive(),
            0,
            "the silent child was killed before the retry"
        );
        tokio::time::sleep(RETRY_BASE).await;
        settle().await;
        assert_eq!(fake.started(), 2);
        assert_eq!(
            heard_now(&a.inbox).await,
            vec![Heard::Listening, Heard::CatchUp]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_forwarding_error_from_a_living_child_restarts_it() {
        let fake = FakeForwarder::default();
        let transports = Transports::new(fake.clone());
        let a = transports.subscribe("o/r", &["issues"]);
        settle().await;
        assert_eq!(
            heard_now(&a.inbox).await,
            vec![Heard::Listening, Heard::CatchUp]
        );
        fake.say("warning: error forwarding event: connection refused");
        settle().await;
        let heard = heard_now(&a.inbox).await;
        assert!(
            matches!(
                &heard[..],
                [Heard::Down(RelayDown {
                    cause: RelayFailure::ForwardingError(_),
                    ..
                })]
            ),
            "{heard:?}"
        );
        assert_eq!(fake.alive(), 0);
        tokio::time::sleep(RETRY_BASE).await;
        settle().await;
        assert_eq!(
            heard_now(&a.inbox).await,
            vec![Heard::Listening, Heard::CatchUp]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_quiet_healthy_relay_is_left_alone() {
        let fake = FakeForwarder::default();
        let transports = Transports::new(fake.clone());
        let a = transports.subscribe("o/r", &["issues"]);
        settle().await;
        assert_eq!(
            heard_now(&a.inbox).await,
            vec![Heard::Listening, Heard::CatchUp]
        );
        tokio::time::sleep(Duration::from_secs(10 * 3600)).await;
        settle().await;
        assert!(heard_now(&a.inbox).await.is_empty());
        assert_eq!(fake.started(), 1, "silence is not a failure");
    }

    #[tokio::test(start_paused = true)]
    async fn a_forwarder_that_cannot_start_names_why() {
        let fake = FakeForwarder::scripted(&[Run::NotStarted("No such file or directory")]);
        let transports = Transports::new(fake.clone());
        let a = transports.subscribe("o/r", &["issues"]);
        settle().await;
        let heard = heard_now(&a.inbox).await;
        let [Heard::Down(down)] = &heard[..] else {
            panic!("{heard:?}");
        };
        assert!(
            down.cause
                .describe("o/r")
                .contains("No such file or directory")
        );
    }
}
