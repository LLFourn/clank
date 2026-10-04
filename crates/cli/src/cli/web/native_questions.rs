//! Connector-owned question subscriptions. Transcript call IDs never authorize
//! answers; these windows contain only requests from the bound native owner.
//! No server spawn, saved-session restoration, or idle question polling.
use std::collections::BTreeMap;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use clank_core::{Tool, ids::AgentLabel};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

use super::Feed;
use super::transcript::questions::Question;

// Negotiate away token/output/history notifications rather than copy a second
// transcript stream merely to receive occasional questions. Native server
// requests, resolution and turn completion remain enabled.
const QUIET_NOTIFICATIONS: &[&str] = &[
    "item/agentMessage/delta",
    "item/plan/delta",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/summaryPartAdded",
    "item/reasoning/textDelta",
    "item/started",
    "item/completed",
    "item/commandExecution/outputDelta",
    "item/commandExecution/terminalInteraction",
    "item/fileChange/outputDelta",
    "item/fileChange/patchUpdated",
    "item/mcpToolCall/progress",
    "thread/tokenUsage/updated",
    "turn/diff/updated",
    "turn/plan/updated",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Request {
    /// The provider's server-request id, unrelated to our client RPC ids.
    #[serde(serialize_with = "opaque_id")]
    pub(crate) id: Value,
    pub(crate) turn: String,
    pub(crate) item: String,
    pub(crate) blocking: bool,
    pub(crate) question: Question,
    pub(crate) delivery: Delivery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Delivery {
    Pending,
    Sending,
    Submitted,
    Answered,
    Local,
    Failed,
    Unknown,
    Unavailable,
}

struct Command {
    ask: clank_web::request::QuestionAnswer,
    reply: oneshot::Sender<clank_web::request::BrowserReply>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Window {
    pub(crate) session: String,
    pub(crate) owner: String,
    pub(crate) available: bool,
    pub(crate) reason: Option<String>,
    pub(crate) pending: Vec<Request>,
    pub(crate) ended: Option<TurnEnd>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TurnEnd {
    pub(crate) turn: String,
    pub(crate) status: String,
}
fn opaque_id<S: serde::Serializer>(
    id: &Value,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&id.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    label: AgentLabel,
    session: clank_core::agent_config::Session,
    socket: Option<(PathBuf, u64, u64)>,
    ambiguous: bool,
}

fn target(repo: &Path, label: &AgentLabel, default_home: Option<&Path>) -> Option<Target> {
    let session = crate::agent_store::load_agent_config(repo, label)
        .ok()??
        .session?;
    if session.tool != Tool::Codex {
        return None;
    }
    let roster = crate::agent_store::try_resolve_via_team(repo).ok()??;
    let desc = if &roster.master == label {
        &roster.master_desc
    } else {
        &roster.members.iter().find(|m| &m.label == label)?.desc
    };
    if desc.tool != Tool::Codex {
        return None;
    }
    let env = |key| desc.launch.as_ref().and_then(|l| l.env.get(key));
    let root = env("CODEX_HOME").map(PathBuf::from).or_else(|| {
        env("HOME")
            .map(PathBuf::from)
            .or_else(|| default_home.map(Path::to_owned))
            .map(|home| home.join(".codex"))
    });
    // Current Codex advertises its daemon socket through a symlink. Resolve
    // that candidate, validate its uid/type, then authorize the thread/cwd.
    // Neither a relative override nor the monitor's CODEX_HOME is a candidate.
    let socket = root.filter(|root| root.is_absolute()).and_then(|root| {
        let path =
            std::fs::canonicalize(root.join("app-server-control/app-server-control.sock")).ok()?;
        let meta = std::fs::metadata(&path).ok()?;
        (meta.file_type().is_socket() && meta.uid() == unsafe { libc::geteuid() })
            .then(|| (path, meta.dev(), meta.ino()))
    });
    Some(Target {
        label: label.clone(),
        session,
        socket,
        ambiguous: false,
    })
}

struct Lane {
    target: Target,
    owner: String,
    commands: mpsc::Sender<Command>,
    abort: Option<tokio::task::AbortHandle>,
}
struct State {
    lanes: BTreeMap<String, Lane>,
    tasks: JoinSet<()>,
    next: u64,
}

pub(crate) struct Owners {
    repo: PathBuf,
    home: Option<PathBuf>,
    feed: Feed,
    epoch: String,
    runtime: tokio::runtime::Handle,
    state: Mutex<State>,
}
impl Owners {
    pub(crate) fn new(repo: PathBuf, home: Option<PathBuf>, feed: Feed) -> Self {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).expect("question owner nonce");
        Self {
            repo,
            home,
            feed,
            epoch: nonce.iter().map(|b| format!("{b:02x}")).collect(),
            runtime: tokio::runtime::Handle::current(),
            state: Mutex::new(State {
                lanes: BTreeMap::new(),
                tasks: JoinSet::new(),
                next: 0,
            }),
        }
    }
    /// Invoked by existing status observations, not a separate timer. An
    /// unchanged target never creates another socket or retries a failed one.
    pub(crate) fn observe(&self, snap: &crate::cli::status::StatusSnapshot) {
        let wanted: BTreeMap<_, _> = snap
            .agents
            .iter()
            .filter_map(|a| {
                let label = AgentLabel::parse(&a.label).ok()?;
                target(&self.repo, &label, self.home.as_deref()).map(|t| (a.label.clone(), t))
            })
            .collect();
        self.reconcile(wanted);
    }
    fn reconcile(&self, mut wanted: BTreeMap<String, Target>) {
        let mut counts = BTreeMap::new();
        for t in wanted.values() {
            *counts.entry(t.session.id.as_str().to_owned()).or_insert(0) += 1;
        }
        for t in wanted.values_mut() {
            t.ambiguous = counts[t.session.id.as_str()] > 1;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.tasks.try_join_next().is_some() {}
        let gone: Vec<_> = state
            .lanes
            .iter()
            .filter(|(label, lane)| wanted.get(*label) != Some(&lane.target))
            .map(|(label, _)| label.clone())
            .collect();
        for label in gone {
            let lane = state.lanes.remove(&label).unwrap();
            // Invalidation precedes cancellation: even a late read cannot
            // republish content after the old binding has been replaced.
            self.feed.question_release(&label, &lane.owner);
            if let Some(abort) = lane.abort {
                abort.abort();
            }
        }
        // Duplicate labels bound to one native thread are not two owners.
        for (label, t) in &wanted {
            if state.lanes.contains_key(label) {
                continue;
            }
            state.next += 1;
            let owner = format!("{}:{}", self.epoch, state.next);
            let ambiguous = t.ambiguous;
            let usable = t.socket.is_some() && !ambiguous;
            let (commands, receiver) = mpsc::channel(1);
            self.feed.question_claim(
                label,
                Some(Window {
                    session: t.session.id.as_str().into(),
                    owner: owner.clone(),
                    available: false,
                    reason: Some(
                        if ambiguous {
                            "This native session is bound to multiple agents."
                        } else if usable {
                            "Connecting to the native question owner."
                        } else {
                            "The bound Codex profile has no accessible native socket."
                        }
                        .into(),
                    ),
                    pending: vec![],
                    ended: None,
                }),
            );
            let abort = if usable {
                Some(self.spawn(&mut state.tasks, t.clone(), owner.clone(), receiver))
            } else {
                None
            };
            state.lanes.insert(
                label.clone(),
                Lane {
                    target: t.clone(),
                    owner,
                    commands,
                    abort,
                },
            );
        }
    }
    fn spawn(
        &self,
        tasks: &mut JoinSet<()>,
        target: Target,
        owner: String,
        receiver: mpsc::Receiver<Command>,
    ) -> tokio::task::AbortHandle {
        let repo = self.repo.clone();
        let home = self.home.clone();
        let feed = self.feed.clone();
        let _entered = self.runtime.enter();
        tasks.spawn(async move {
            let current = || target_is_current(&repo, &target, home.as_deref());
            if let Err(error) = subscribe(&repo, &target, &feed, &owner, receiver, current).await {
                feed.question_update(target.label.as_str(), &owner, |window| {
                    window.available = false;
                    // Preserve delivery fences across explicit recovery. A failed
                    // write must never turn into fresh answer authority on replay.
                    window.reason = Some(format!("Native question connection unavailable: {error}. Reconnect or use Terminal."));
                });
            }
        })
    }
    pub(crate) fn reconnect(
        &self,
        ask: clank_web::request::QuestionReconnect,
    ) -> clank_web::request::BrowserReply {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.tasks.try_join_next().is_some() {}
        let Some(lane) = state.lanes.get(&ask.agent) else {
            return answer_reply(409, "stale", "The native owner changed.");
        };
        if lane.owner != ask.owner
            || lane.target.session.id.as_str() != ask.session
            || !target_is_current(&self.repo, &lane.target, self.home.as_deref())
        {
            return answer_reply(
                409,
                "stale",
                "The binding or native socket changed. Wait for the current owner.",
            );
        }
        if lane.abort.as_ref().is_some_and(|task| !task.is_finished()) {
            return answer_reply(
                409,
                "busy",
                "A native connection operation is already active. Wait for it to finish.",
            );
        }
        if lane.target.socket.is_none() || lane.target.ambiguous {
            return answer_reply(
                409,
                "unavailable",
                "No unambiguous native socket is available. Use Terminal.",
            );
        }
        let target = lane.target.clone();
        let owner = lane.owner.clone();
        let (commands, receiver) = mpsc::channel(1);
        self.feed.question_update(&ask.agent, &owner, |window| {
            window.available = false;
            window.reason = Some("Reconnecting to the native question owner.".into());
        });
        let abort = self.spawn(&mut state.tasks, target, owner, receiver);
        let lane = state.lanes.get_mut(&ask.agent).unwrap();
        lane.commands = commands;
        lane.abort = Some(abort);
        answer_reply(202, "connecting", "Reconnecting. No answer was submitted.")
    }
    pub(crate) async fn answer(
        &self,
        ask: clank_web::request::QuestionAnswer,
    ) -> clank_web::request::BrowserReply {
        let commands = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(lane) = state.lanes.get(&ask.agent).filter(|lane| {
                lane.owner == ask.owner && lane.target.session.id.as_str() == ask.session
            }) else {
                return answer_reply(
                    409,
                    "stale",
                    "The native question owner changed. Your answer was not sent.",
                );
            };
            lane.commands.clone()
        };
        let (reply, response) = oneshot::channel();
        match commands.try_send(Command {ask, reply}) {
            Ok(()) => response.await.unwrap_or_else(|_| answer_reply(503, "unknown", "The native owner stopped. Delivery is unconfirmed; check Terminal before trying again.")),
            Err(mpsc::error::TrySendError::Full(_)) => answer_reply(409, "busy", "Another native answer is in progress. Your answer was not sent."),
            Err(mpsc::error::TrySendError::Closed(_)) => answer_reply(409, "unavailable", "The native question connection is unavailable. Your answer was not sent."),
        }
    }
    pub(crate) fn stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for (label, lane) in std::mem::take(&mut state.lanes) {
            self.feed.question_release(&label, &lane.owner);
            if let Some(abort) = lane.abort {
                abort.abort();
            }
        }
        state.tasks.abort_all();
    }
    pub(crate) async fn join(&self) {
        // No tasks are created after BridgeInstance starts stopping.
        let mut tasks = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut state.tasks)
        };
        while tasks.join_next().await.is_some() {}
    }
}
impl Drop for Owners {
    fn drop(&mut self) {
        self.stop();
    }
}
fn target_is_current(repo: &Path, expected: &Target, home: Option<&Path>) -> bool {
    if target(repo, &expected.label, home).as_ref() != Some(expected) {
        return false;
    }
    let Some(roster) = crate::agent_store::try_resolve_via_team(repo)
        .ok()
        .flatten()
    else {
        return false;
    };
    // A second binding may have appeared since the last status observation.
    // Recheck that ambiguity before a native response can be written.
    std::iter::once(&roster.master)
        .chain(roster.members.iter().map(|m| &m.label))
        .filter(|label| *label != &expected.label)
        .all(
            |label| match crate::agent_store::load_agent_config(repo, label) {
                Ok(cfg) => !cfg.and_then(|cfg| cfg.session).is_some_and(|session| {
                    session.tool == Tool::Codex && session.id == expected.session.id
                }),
                Err(_) => false,
            },
        )
}

struct Native<S> {
    socket: WebSocketStream<S>,
    next: u64,
    waiting: Vec<Value>,
}
impl<S: AsyncRead + AsyncWrite + Unpin> Native<S> {
    async fn send(&mut self, value: Value) -> Result<()> {
        self.socket
            .send(Message::Text(value.to_string().into()))
            .await?;
        Ok(())
    }
    async fn receive(&mut self) -> Result<Value> {
        while let Some(frame) = self.socket.next().await {
            if let Message::Text(text) = frame? {
                return Ok(serde_json::from_str(&text)?);
            }
        }
        bail!("native socket closed")
    }
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        let id = format!("clank-question-{}", self.next);
        self.send(json!({"id":id,"method":method,"params":params}))
            .await?;
        loop {
            let value = self.receive().await?;
            if value.get("method").is_none() && value["id"] == id {
                ensure!(value.get("error").is_none(), "native {method} rejected");
                return value
                    .get("result")
                    .cloned()
                    .context("native reply has no result");
            }
            // Resume may replay before its RPC result. Retain native question
            // lifecycle events; irrelevant transcript/usage events need no copy.
            if relevant(&value) {
                self.waiting.push(value);
            }
        }
    }
    async fn attach(
        &mut self,
        repo: &Path,
        thread: &str,
        current: impl Fn() -> bool,
    ) -> Result<()> {
        self.call("initialize",json!({"clientInfo":{"name":"clank-questions","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true,"optOutNotificationMethods":QUIET_NOTIFICATIONS}})).await?;
        self.send(json!({"method":"initialized","params":{}}))
            .await?;
        let loaded = self.call("thread/loaded/list", json!({})).await?;
        ensure!(
            loaded["data"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|id| id == thread)),
            "bound thread is not already loaded"
        );
        let read = self
            .call(
                "thread/read",
                json!({"threadId":thread,"includeTurns":false}),
            )
            .await?;
        let cwd = read["thread"]["cwd"]
            .as_str()
            .context("thread cwd missing")?;
        ensure!(
            read["thread"]["id"] == thread
                && dunce::canonicalize(cwd)? == dunce::canonicalize(repo)?,
            "thread belongs to another workspace"
        );
        ensure!(current(), "binding changed before subscription");
        self.call(
            "thread/resume",
            json!({"threadId":thread,"excludeTurns":true}),
        )
        .await?;
        ensure!(current(), "binding changed while subscribing");
        Ok(())
    }
}
fn relevant(value: &Value) -> bool {
    matches!(
        value["method"].as_str(),
        Some("item/tool/requestUserInput" | "serverRequest/resolved" | "turn/completed")
    )
}

async fn subscribe(
    repo: &Path,
    target: &Target,
    feed: &Feed,
    owner: &str,
    mut commands: mpsc::Receiver<Command>,
    current: impl Fn() -> bool,
) -> Result<()> {
    // This deadline bounds only attachment to an unresponsive native server;
    // the healthy idle reader has no timeout or periodic work.
    let mut native = tokio::time::timeout(Duration::from_secs(10), async {
        let path = &target.socket.as_ref().context("no native socket")?.0;
        let stream = tokio::net::UnixStream::connect(path).await?;
        let (socket, _) = tokio_tungstenite::client_async("ws://localhost/", stream).await?;
        let mut native = Native {
            socket,
            next: 0,
            waiting: vec![],
        };
        native
            .attach(repo, target.session.id.as_str(), &current)
            .await?;
        Ok::<_, anyhow::Error>(native)
    })
    .await
    .context("native attachment timed out")??;
    // Resume replays pending requests. Rebuild the pending set from that replay,
    // retaining prior delivery fences only for the exact same request identity.
    let mut previous = feed
        .question_window(target.label.as_str())
        .map(|w| w.pending)
        .unwrap_or_default();
    feed.question_update(target.label.as_str(), owner, |w| w.pending.clear());
    for value in std::mem::take(&mut native.waiting) {
        ensure!(current(), "binding changed");
        apply(
            feed,
            target.label.as_str(),
            owner,
            target.session.id.as_str(),
            value,
        )?;
    }
    restore_delivery(feed, target.label.as_str(), owner, &previous);
    feed.question_update(target.label.as_str(), owner, |w| {
        w.available = true;
        w.reason = None;
    });
    let mut attempts = BTreeMap::new();
    loop {
        tokio::select! {
            // Prefer a resolution already waiting on the socket over another
            // queued answer. The provider still arbitrates a simultaneous local
            // response; neither socket writes nor resolution establish victory.
            biased;
            value = native.receive() => {
                let value = value?;
                if relevant(&value) {
                    ensure!(current(), "binding changed");
                    if value["method"] == "serverRequest/resolved" {
                        previous.retain(|r| r.id != value["params"]["requestId"]);
                    } else if value["method"] == "turn/completed" {
                        previous.retain(|r| r.turn != value["params"]["turn"]["id"]);
                    }
                    apply(feed, target.label.as_str(), owner, target.session.id.as_str(), value)?;
                    restore_delivery(feed, target.label.as_str(), owner, &previous);
                    let active = feed.question_window(target.label.as_str()).map(|w| w.pending.into_iter().map(|r|r.id.to_string()).collect::<std::collections::BTreeSet<_>>()).unwrap_or_default();
                    attempts.retain(|id, _| active.contains(id));
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { return Ok(()); };
                send_answer(&mut native, feed, target.label.as_str(), owner, target.session.id.as_str(), command, &current, &mut attempts).await?;
            }
        }
    }
}

fn restore_delivery(feed: &Feed, label: &str, owner: &str, previous: &[Request]) {
    feed.question_update(label, owner, |w| {
        for request in &mut w.pending {
            if let Some(old) = previous.iter().find(|old| {
                old.id == request.id
                    && old.turn == request.turn
                    && old.item == request.item
                    && old.question == request.question
            }) {
                if request.delivery == Delivery::Pending && old.delivery != Delivery::Pending {
                    request.delivery = old.delivery;
                }
            }
        }
    });
}

pub(super) fn answer_reply(
    status: u16,
    state: &str,
    message: &str,
) -> clank_web::request::BrowserReply {
    clank_web::request::BrowserReply {
        status,
        content: clank_web::request::Content::Json,
        body: json!({"state":state,"message":message}).to_string(),
    }
}

pub(super) fn answer_payload(
    request: &Request,
    ask: &clank_web::request::QuestionAnswer,
) -> Result<Value> {
    ensure!(
        !ask.attempt.trim().is_empty(),
        "answer attempt identity is missing"
    );
    ensure!(
        ask.answers.len() == request.question.fields.len(),
        "answer every question using its field ID"
    );
    let mut answers = serde_json::Map::new();
    for field in &request.question.fields {
        let id = field
            .id
            .as_deref()
            .filter(|id| !id.is_empty())
            .context("native field identity missing")?;
        let values = ask.answers.get(id).context("answer field ID changed")?;
        ensure!(
            !values.is_empty() && (field.multi_select || values.len() == 1),
            "invalid number of answers for a question"
        );
        let unique: std::collections::BTreeSet<_> = values.iter().collect();
        ensure!(unique.len() == values.len(), "duplicate answer choices");
        for value in values {
            ensure!(!value.trim().is_empty(), "an answer cannot be blank");
            ensure!(
                field.options.is_empty()
                    || field.is_other
                    || field.options.iter().any(|option| option.label == *value),
                "answer is not an offered choice"
            );
        }
        answers.insert(id.into(), json!({"answers":values}));
    }
    Ok(json!({"id":request.id,"result":{"answers":answers}}))
}

async fn send_answer<S: AsyncRead + AsyncWrite + Unpin>(
    native: &mut Native<S>,
    feed: &Feed,
    label: &str,
    owner: &str,
    thread: &str,
    command: Command,
    current: impl Fn() -> bool,
    attempts: &mut BTreeMap<String, (String, blake3::Hash)>,
) -> Result<()> {
    let Command { ask, reply } = command;
    if reply.is_closed() {
        return Ok(());
    }
    let window = feed.question_window(label);
    let request = window
        .as_ref()
        .filter(|w| {
            w.available
                && w.owner == owner
                && w.session == thread
                && ask.agent == label
                && ask.owner == owner
                && ask.session == thread
        })
        .and_then(|w| {
            w.pending
                .iter()
                .find(|r| r.id.to_string() == ask.id && r.turn == ask.turn && r.item == ask.item)
        });
    let Some(request) = request else {
        let _ = reply.send(answer_reply(
            409,
            "stale",
            "The native request is no longer pending. Your answer was not sent.",
        ));
        return Ok(());
    };
    if !current() {
        let _ = reply.send(answer_reply(
            409,
            "stale",
            "The agent binding changed. Your answer was not sent.",
        ));
        bail!("binding changed before answer");
    }
    let payload = match answer_payload(request, &ask) {
        Ok(payload) => payload,
        Err(error) => {
            let _ = reply.send(answer_reply(400, "invalid", &error.to_string()));
            return Ok(());
        }
    };
    // Only a digest is kept locally for idempotency. No answer strings enter
    // the retained browser feed, logs, storage or another viewer's replay.
    let digest = blake3::hash(&serde_json::to_vec(&payload)?);
    let key = request.id.to_string();
    if let Some((prior, hash)) = attempts.get(&key) {
        let same = *prior == ask.attempt && *hash == digest;
        let _=reply.send(if same {answer_reply(202,"submitted","Answer sent; its outcome is unconfirmed. Check Terminal for the result.")}
            else {answer_reply(409,"submitted","An answer was already sent for this request. Check Terminal before taking further action.")});
        return Ok(());
    }
    if request.delivery != Delivery::Pending {
        let _ = reply.send(answer_reply(409, "unknown", "An answer was previously attempted. Its outcome is unconfirmed; check Terminal. No answer was sent."));
        return Ok(());
    }
    attempts.insert(key, (ask.attempt, digest));
    feed.question_update(label, owner, |w| {
        if let Some(r) = w.pending.iter_mut().find(|r| r.id == request.id) {
            r.delivery = Delivery::Sending;
        }
    });
    let sent = tokio::time::timeout(Duration::from_secs(10), native.send(payload)).await;
    match sent {
        Ok(Ok(())) => {
            feed.question_update(label, owner, |w| {
                if let Some(r) = w.pending.iter_mut().find(|r| r.id == request.id) {
                    r.delivery = Delivery::Submitted;
                }
            });
            let _ = reply.send(answer_reply(
                202,
                "submitted",
                "Answer sent; awaiting the harness. Delivery is not proof it was accepted.",
            ));
            Ok(())
        }
        _ => {
            let _ = reply.send(answer_reply(
                502,
                "unknown",
                "Answer delivery is unconfirmed. Check Terminal before trying again.",
            ));
            bail!("answer delivery unconfirmed; connection closed to prevent another attempt")
        }
    }
}
fn apply(feed: &Feed, label: &str, owner: &str, thread: &str, value: Value) -> Result<()> {
    let params = &value["params"];
    if params["threadId"] != thread {
        return Ok(());
    }
    match value["method"].as_str() {
        Some("item/tool/requestUserInput") => {
            let id = &value["id"];
            ensure!(
                id.is_string() || id.as_i64().is_some() || id.as_u64().is_some(),
                "invalid native request id"
            );
            let turn = params["turnId"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("question turn missing")?;
            let item = params["itemId"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("question item missing")?;
            let blocking = params["isBlocking"]
                .as_bool()
                .context("question blocking flag missing")?;
            let question = super::transcript::questions::parse(
                Tool::Codex,
                "request_user_input",
                &params.to_string(),
            )
            .context("unsupported native question fields")?;
            let ids: std::collections::BTreeSet<_> = question
                .fields
                .iter()
                .filter_map(|field| field.id.as_deref().filter(|id| !id.is_empty()))
                .collect();
            ensure!(
                ids.len() == question.fields.len(),
                "native question field IDs are missing or duplicated"
            );
            let request = Request {
                id: id.clone(),
                turn: turn.into(),
                item: item.into(),
                blocking,
                question,
                delivery: Delivery::Pending,
            };
            let mut conflict = false;
            feed.question_update(label, owner, |w| {
                if let Some(old) = w.pending.iter().find(|old| old.id == *id) {
                    let mut replay = request.clone();
                    replay.delivery = old.delivery;
                    conflict = old != &replay;
                } else {
                    w.pending.push(request);
                }
            });
            ensure!(!conflict, "native request id reused with different content");
        }
        Some("serverRequest/resolved") => feed.question_update(label, owner, |w| {
            w.pending.retain(|r| r.id != params["requestId"])
        }),
        Some("turn/completed") => feed.question_update(label, owner, |w| {
            w.pending.retain(|r| r.turn != params["turn"]["id"]);
            if let (Some(turn), Some(status)) = (
                params["turn"]["id"].as_str(),
                params["turn"]["status"].as_str(),
            ) {
                w.ended = Some(TurnEnd {
                    turn: turn.into(),
                    status: status.into(),
                });
            }
        }),
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::protocol::Role;
    fn window(owner: &str) -> Window {
        Window {
            session: "thread".into(),
            owner: owner.into(),
            available: true,
            reason: None,
            pending: vec![],
            ended: None,
        }
    }
    #[tokio::test]
    async fn unavailable_and_ambiguous_targets_stay_quiet_on_unchanged_observations() {
        use clank_core::{agent_config::Session, ids::SessionId};
        let repo = tempfile::tempdir().unwrap();
        let feed = Feed::new();
        let owners = Owners::new(repo.path().into(), None, feed.clone());
        let first = Target {
            label: AgentLabel::parse("first").unwrap(),
            session: Session {
                id: SessionId::parse("thread-a1").unwrap(),
                tool: Tool::Codex,
                updated_at: "binding-one".into(),
                transcript: None,
            },
            socket: None,
            ambiguous: false,
        };
        let wanted = BTreeMap::from([("first".into(), first.clone())]);
        owners.reconcile(wanted.clone());
        let initial = feed.snapshot().questions["first"].clone();
        let (_, mut updates) = feed.bridge_connect();
        owners.reconcile(wanted);
        assert_eq!(feed.snapshot().questions["first"], initial);
        assert!(updates.try_recv().is_err());
        assert!(owners.state.lock().unwrap().tasks.is_empty());
        let mut second = first.clone();
        second.label = AgentLabel::parse("second").unwrap();
        owners.reconcile(BTreeMap::from([
            ("first".into(), first),
            ("second".into(), second),
        ]));
        let windows = feed.snapshot().questions;
        assert_ne!(windows["first"].owner, initial.owner);
        assert!(
            windows
                .values()
                .all(|w| !w.available && w.reason.as_deref().unwrap().contains("multiple agents"))
        );
        assert!(owners.state.lock().unwrap().tasks.is_empty());
        owners.stop();
        owners.join().await;
        assert!(feed.snapshot().questions.is_empty());
    }
    fn question(id: Value) -> Value {
        json!({"id":id,"method":"item/tool/requestUserInput","params":{
            "threadId":"thread","turnId":"turn","itemId":"item","isBlocking":false,
            "questions":[{"id":"choice","header":"Choice","question":"Which path?",
                "options":[{"label":"First","description":"Path one"}]}]}})
    }
    #[test]
    fn live_questions_replay_exact_identity_and_obsolete_owners_cannot_publish() {
        let feed = Feed::new();
        feed.question_claim("agent", Some(window("old")));
        let request = question(json!(7));
        apply(&feed, "agent", "old", "thread", request.clone()).unwrap();
        apply(&feed, "agent", "old", "thread", request.clone()).unwrap();
        let pending = &feed.snapshot().questions["agent"].pending;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, json!(7));
        assert_eq!(pending[0].turn, "turn");
        assert_eq!(pending[0].item, "item");
        assert!(!pending[0].blocking);
        let (image, _) = feed.bridge_connect();
        let frames: clank_web::service::BrowserFrames =
            serde_json::from_value(image.state).unwrap();
        let replay = frames
            .replay
            .iter()
            .find(|frame| frame.event == "questions")
            .unwrap();
        assert_eq!(replay.data["agents"]["agent"]["pending"][0]["id"], "7");
        feed.question_claim("agent", Some(window("new")));
        apply(&feed, "agent", "old", "thread", request.clone()).unwrap();
        assert!(feed.snapshot().questions["agent"].pending.is_empty());
        apply(&feed, "agent", "new", "thread", request.clone()).unwrap();
        apply(
            &feed,
            "agent",
            "old",
            "thread",
            json!({"method":"serverRequest/resolved","params":{"threadId":"thread","requestId":7}}),
        )
        .unwrap();
        assert_eq!(feed.snapshot().questions["agent"].pending.len(), 1);
        feed.question_claim("agent", None);
        apply(&feed, "agent", "new", "thread", request).unwrap();
        assert!(feed.snapshot().questions.is_empty());
    }
    #[test]
    fn resolution_and_cancel_remove_pending_without_inventing_an_answer() {
        let feed = Feed::new();
        feed.question_claim("agent", Some(window("owner")));
        apply(&feed, "agent", "owner", "thread", question(json!("native"))).unwrap();
        let resolve = |thread| json!({"method":"serverRequest/resolved","params":{"threadId":thread,"requestId":"native"}});
        apply(&feed, "agent", "owner", "thread", resolve("foreign")).unwrap();
        assert_eq!(feed.snapshot().questions["agent"].pending.len(), 1);
        apply(&feed, "agent", "owner", "thread", resolve("thread")).unwrap();
        assert!(feed.snapshot().questions["agent"].pending.is_empty());
        apply(&feed, "agent", "owner", "thread", question(json!(9))).unwrap();
        apply(
            &feed,
            "agent",
            "owner",
            "thread",
            json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"other"}}}),
        )
        .unwrap();
        assert_eq!(feed.snapshot().questions["agent"].pending.len(), 1);
        apply(&feed,"agent","owner","thread",json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"turn","status":"interrupted"}}})).unwrap();
        assert!(feed.snapshot().questions["agent"].pending.is_empty());
        assert!(feed.snapshot().questions["agent"].available);
    }
    #[test]
    fn malformed_requests_and_reused_ids_are_not_answer_authority() {
        let feed = Feed::new();
        feed.question_claim("agent", Some(window("owner")));
        let mut bad = question(json!(1));
        bad["params"]["isBlocking"] = Value::Null;
        assert!(apply(&feed, "agent", "owner", "thread", bad).is_err());
        assert!(feed.snapshot().questions["agent"].pending.is_empty());
        apply(&feed, "agent", "owner", "thread", question(json!(1))).unwrap();
        let mut changed = question(json!(1));
        changed["params"]["turnId"] = json!("replacement");
        assert!(apply(&feed, "agent", "owner", "thread", changed).is_err());
        assert_eq!(feed.snapshot().questions["agent"].pending[0].turn, "turn");
    }
    async fn pair() -> (
        Native<tokio::io::DuplexStream>,
        WebSocketStream<tokio::io::DuplexStream>,
    ) {
        let (client, server) = tokio::io::duplex(16384);
        (
            Native {
                socket: WebSocketStream::from_raw_socket(client, Role::Client, None).await,
                next: 0,
                waiting: vec![],
            },
            WebSocketStream::from_raw_socket(server, Role::Server, None).await,
        )
    }
    fn ask(owner: &str) -> clank_web::request::QuestionAnswer {
        clank_web::request::QuestionAnswer {
            agent: "agent".into(),
            session: "thread".into(),
            owner: owner.into(),
            id: "7".into(),
            turn: "turn".into(),
            item: "item".into(),
            attempt: "attempt-one".into(),
            answers: BTreeMap::from([("choice".into(), vec!["First".into()])]),
        }
    }
    #[test]
    fn recovered_delivery_never_reopens_an_attempted_answer() {
        let feed = Feed::new();
        let owner = "recovery-owner";
        let mut request = Request {
            id: json!(77), turn: "turn".into(), item: "item".into(), blocking: false,
            question: super::super::transcript::questions::parse(Tool::Codex, "request_user_input",
                &json!({"questions":[{"id":"choice","header":"Pick","question":"Pick one","options":[{"label":"First","description":"first"}]}]}).to_string()).unwrap(),
            delivery: Delivery::Submitted,
        };
        let previous = vec![request.clone()];
        request.delivery = Delivery::Pending;
        feed.question_claim(
            "codex",
            Some(Window {
                session: "thread".into(),
                owner: owner.into(),
                available: true,
                reason: None,
                pending: vec![request],
                ended: None,
            }),
        );
        restore_delivery(&feed, "codex", owner, &previous);
        assert_eq!(
            feed.question_window("codex").unwrap().pending[0].delivery,
            Delivery::Submitted
        );
        // An originally unsent draft cannot downgrade a subsequent submission.
        let mut old = previous[0].clone();
        old.delivery = Delivery::Pending;
        restore_delivery(&feed, "codex", owner, &[old]);
        assert_eq!(
            feed.question_window("codex").unwrap().pending[0].delivery,
            Delivery::Submitted
        );
    }

    #[test]
    fn answers_validate_native_fields_choices_and_preserve_untruncated_private_text() {
        let feed = Feed::new();
        feed.question_claim("agent", Some(window("owner")));
        let mut native = question(json!(7));
        native["params"]["questions"] = json!([
            {"id":"choice","question":"Choices?","multiSelect":true,"options":[{"label":"First","description":"One"},{"label":"Second","description":"Two"}]},
            {"id":"private","question":"Private text?","isSecret":true,"options":null},
            {"id":"other","question":"Other?","isOther":true,"options":[{"label":"Default","description":"Default"}]}
        ]);
        apply(&feed, "agent", "owner", "thread", native).unwrap();
        let request = feed.question_window("agent").unwrap().pending.remove(0);
        let mut answer = ask("owner");
        let private = "Private fixture content. ".repeat(5000);
        answer.answers = BTreeMap::from([
            ("choice".into(), vec!["First".into(), "Second".into()]),
            ("private".into(), vec![private.clone()]),
            ("other".into(), vec!["A custom answer".into()]),
        ]);
        let payload = answer_payload(&request, &answer).unwrap();
        assert_eq!(payload["id"], 7);
        assert_eq!(
            payload["result"]["answers"]["private"]["answers"][0],
            private
        );
        let retained = serde_json::to_string(&feed.question_window("agent")).unwrap();
        assert!(!retained.contains("Private fixture content"));
        let mut invalid = answer.clone();
        invalid
            .answers
            .get_mut("choice")
            .unwrap()
            .push("First".into());
        assert!(answer_payload(&request, &invalid).is_err());
        invalid = answer.clone();
        invalid
            .answers
            .insert("choice".into(), vec!["Foreign option".into()]);
        assert!(answer_payload(&request, &invalid).is_err());
        invalid = answer.clone();
        invalid.answers.remove("private");
        assert!(answer_payload(&request, &invalid).is_err());
        invalid = answer;
        invalid.answers.insert("private".into(), vec![" ".into()]);
        assert!(answer_payload(&request, &invalid).is_err());
    }
    #[tokio::test]
    async fn stale_identity_or_binding_cannot_send_and_socket_error_is_unknown() {
        for changed_binding in [false, true] {
            let feed = Feed::new();
            feed.question_claim("agent", Some(window("owner")));
            apply(&feed, "agent", "owner", "thread", question(json!(7))).unwrap();
            let (mut native, mut server) = pair().await;
            let (reply, response) = oneshot::channel();
            let mut answer = ask("owner");
            if !changed_binding {
                answer.turn = "foreign-turn".into();
            }
            let mut attempts = BTreeMap::new();
            let result = send_answer(
                &mut native,
                &feed,
                "agent",
                "owner",
                "thread",
                Command { ask: answer, reply },
                || !changed_binding,
                &mut attempts,
            )
            .await;
            assert_eq!(response.await.unwrap().status, 409);
            assert_eq!(result.is_err(), changed_binding);
            assert!(attempts.is_empty());
            assert!(
                tokio::time::timeout(Duration::from_millis(20), server.next())
                    .await
                    .is_err()
            );
        }
        let feed = Feed::new();
        feed.question_claim("agent", Some(window("owner")));
        apply(&feed, "agent", "owner", "thread", question(json!(7))).unwrap();
        let (mut native, server) = pair().await;
        drop(server);
        let (reply, response) = oneshot::channel();
        let mut attempts = BTreeMap::new();
        assert!(
            send_answer(
                &mut native,
                &feed,
                "agent",
                "owner",
                "thread",
                Command {
                    ask: ask("owner"),
                    reply
                },
                || true,
                &mut attempts
            )
            .await
            .is_err()
        );
        let outcome = response.await.unwrap();
        assert_eq!(outcome.status, 502);
        assert_eq!(
            serde_json::from_str::<Value>(&outcome.body).unwrap()["state"],
            "unknown"
        );
        assert_eq!(
            attempts.len(),
            1,
            "record possible mutation before awaiting transport"
        );
    }
    async fn request<S: AsyncRead + AsyncWrite + Unpin>(
        server: &mut WebSocketStream<S>,
        method: &str,
        result: Value,
    ) -> Value {
        let Message::Text(text) = server.next().await.unwrap().unwrap() else {
            panic!("text RPC expected")
        };
        let req: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(req["method"], method);
        if let Some(id) = req.get("id") {
            server
                .send(Message::Text(
                    json!({"id":id,"result":result}).to_string().into(),
                ))
                .await
                .unwrap();
        }
        req
    }
    #[tokio::test]
    async fn attach_retains_replay_before_rpc_reply_and_never_sends_a_tool_response() {
        let repo = tempfile::tempdir().unwrap();
        let (mut native, mut server) = pair().await;
        let (result, ()) = tokio::join!(native.attach(repo.path(), "thread", || true), async {
            let init = request(&mut server, "initialize", json!({})).await;
            let suppressed = init["params"]["capabilities"]["optOutNotificationMethods"]
                .as_array()
                .unwrap();
            assert!(
                suppressed
                    .iter()
                    .any(|method| method == "item/agentMessage/delta")
            );
            assert!(
                !suppressed
                    .iter()
                    .any(|method| method == "serverRequest/resolved" || method == "turn/completed")
            );
            request(&mut server, "initialized", Value::Null).await;
            request(
                &mut server,
                "thread/loaded/list",
                json!({"data":["thread"]}),
            )
            .await;
            request(
                &mut server,
                "thread/read",
                json!({"thread":{"id":"thread","cwd":repo.path()}}),
            )
            .await;
            let Message::Text(text) = server.next().await.unwrap().unwrap() else {
                panic!()
            };
            let req: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(req["method"], "thread/resume");
            assert_eq!(
                req["params"],
                json!({"threadId":"thread","excludeTurns":true})
            );
            server
                .send(Message::Text(question(json!(3)).to_string().into()))
                .await
                .unwrap();
            server
                .send(Message::Text(
                    json!({"id":req["id"],"result":{}}).to_string().into(),
                ))
                .await
                .unwrap();
        });
        result.unwrap();
        assert_eq!(native.waiting, vec![question(json!(3))]);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), server.next())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn foreign_workspace_unloaded_thread_or_changed_binding_never_resume() {
        for scenario in ["foreign", "unloaded", "rebound"] {
            let repo = tempfile::tempdir().unwrap();
            let foreign = tempfile::tempdir().unwrap();
            let (mut native, mut server) = pair().await;
            let (result, ()) = tokio::join!(
                native.attach(repo.path(), "thread", || scenario != "rebound"),
                async {
                    request(&mut server, "initialize", json!({})).await;
                    request(&mut server, "initialized", Value::Null).await;
                    request(&mut server,"thread/loaded/list",json!({"data":if scenario=="unloaded" {vec!["other"]}else {vec!["thread"]}})).await;
                    if scenario != "unloaded" {
                        request(&mut server,"thread/read",json!({"thread":{"id":"thread","cwd":if scenario=="foreign" {foreign.path()}else {repo.path()}}})).await;
                    }
                }
            );
            assert!(result.is_err(), "{scenario}");
            assert!(
                tokio::time::timeout(Duration::from_millis(20), server.next())
                    .await
                    .is_err()
            );
        }
    }
    #[tokio::test]
    async fn profile_selected_socket_subscription_is_single_owned_and_invalidates_on_rebind() {
        use crate::cli::teams_config::{AgentDescription, RosterRole};
        use clank_core::{agent_config::LaunchConfig, ids::SessionId};
        let repo = tempfile::tempdir_in("/tmp").unwrap();
        let profile = tempfile::tempdir_in("/tmp").unwrap();
        let label = AgentLabel::parse("question-agent").unwrap();
        let mut launch = LaunchConfig::default();
        launch
            .env
            .insert("CODEX_HOME".into(), profile.path().to_str().unwrap().into());
        crate::cli::agent::add_repo_roster_agent(
            repo.path(),
            &label,
            AgentDescription {
                tool: Tool::Codex,
                launch: Some(launch),
                initial_prompt: None,
            },
            RosterRole::Master,
        )
        .unwrap();
        crate::agent_store::bind_session_to_agent(
            repo.path(),
            &label,
            Tool::Codex,
            &SessionId::parse("thread-a1").unwrap(),
        )
        .unwrap();
        let socket = profile.path().join("native.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let advert = profile
            .path()
            .join("app-server-control/app-server-control.sock");
        std::fs::create_dir_all(advert.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&socket, &advert).unwrap();
        let target = target(repo.path(), &label, None).unwrap();
        assert_eq!(
            target.socket.as_ref().unwrap().0,
            socket.canonicalize().unwrap()
        );
        let wanted = BTreeMap::from([(label.as_str().to_owned(), target)]);
        let feed = Feed::new();
        let owners = Owners::new(repo.path().into(), None, feed.clone());
        owners.reconcile(wanted.clone());
        let (stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut server = tokio_tungstenite::accept_async(stream).await.unwrap();
        request(&mut server, "initialize", json!({})).await;
        request(&mut server, "initialized", Value::Null).await;
        request(
            &mut server,
            "thread/loaded/list",
            json!({"data":["thread-a1"]}),
        )
        .await;
        request(
            &mut server,
            "thread/read",
            json!({"thread":{"id":"thread-a1","cwd":repo.path()}}),
        )
        .await;
        request(&mut server, "thread/resume", json!({})).await;
        let (_, mut updates) = feed.bridge_connect();
        let mut live_question = question(json!(77));
        live_question["params"]["threadId"] = json!("thread-a1");
        server
            .send(Message::Text(live_question.to_string().into()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while feed.snapshot().questions[label.as_str()].pending.is_empty() {
                match updates.recv().await {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(error) => panic!("fixture update stream closed: {error}"),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            feed.snapshot().questions[label.as_str()].pending[0].id,
            json!(77)
        );
        let ask = clank_web::request::QuestionAnswer {
            agent: label.as_str().into(),
            session: "thread-a1".into(),
            owner: feed.snapshot().questions[label.as_str()].owner.clone(),
            id: "77".into(),
            turn: "turn".into(),
            item: "item".into(),
            attempt: "attempt-one".into(),
            answers: BTreeMap::from([("choice".into(), vec!["First".into()])]),
        };
        let mut invalid = ask.clone();
        invalid.answers = BTreeMap::from([("foreign".into(), vec!["First".into()])]);
        assert_eq!(owners.answer(invalid).await.status, 400);
        let (reply, sent) = tokio::join!(owners.answer(ask.clone()), async {
            let Message::Text(text) = server.next().await.unwrap().unwrap() else {
                panic!()
            };
            serde_json::from_str::<Value>(&text).unwrap()
        });
        assert_eq!(reply.status, 202);
        assert_eq!(
            sent,
            json!({"id":77,"result":{"answers":{"choice":{"answers":["First"]}}}})
        );
        assert_eq!(
            feed.snapshot().questions[label.as_str()].pending[0].delivery,
            Delivery::Submitted
        );
        assert_eq!(owners.answer(ask.clone()).await.status, 202);
        let mut competing = ask.clone();
        competing.attempt = "another-browser".into();
        assert_eq!(owners.answer(competing).await.status, 409);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), server.next())
                .await
                .is_err()
        );
        // Explicit recovery after a dropped socket keeps the owner and the
        // attempted-answer fence, including replay arriving after resume reply.
        drop(server);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if owners.state.lock().unwrap().lanes[label.as_str()]
                    .abort
                    .as_ref()
                    .unwrap()
                    .is_finished()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let recovery = clank_web::request::QuestionReconnect {
            agent: label.as_str().into(),
            session: "thread-a1".into(),
            owner: ask.owner.clone(),
        };
        let mut stale = recovery.clone();
        stale.owner = "other".into();
        assert_eq!(owners.reconnect(stale).status, 409);
        assert_eq!(owners.reconnect(recovery.clone()).status, 202);
        assert_eq!(
            owners.reconnect(recovery).status,
            409,
            "one attachment only"
        );
        let (stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut server = tokio_tungstenite::accept_async(stream).await.unwrap();
        request(&mut server, "initialize", json!({})).await;
        request(&mut server, "initialized", Value::Null).await;
        request(
            &mut server,
            "thread/loaded/list",
            json!({"data":["thread-a1"]}),
        )
        .await;
        request(
            &mut server,
            "thread/read",
            json!({"thread":{"id":"thread-a1","cwd":repo.path()}}),
        )
        .await;
        request(&mut server, "thread/resume", json!({})).await;
        server
            .send(Message::Text(live_question.to_string().into()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let w = feed.question_window(label.as_str()).unwrap();
                if w.available
                    && w.pending
                        .first()
                        .is_some_and(|r| r.delivery == Delivery::Submitted)
                {
                    break;
                }
                match updates.recv().await {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(e) => panic!("{e}"),
                }
            }
        })
        .await
        .unwrap();
        let reply = owners.answer(ask.clone()).await;
        assert_eq!(reply.status, 409);
        assert_eq!(
            serde_json::from_str::<Value>(&reply.body).unwrap()["state"],
            "unknown"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), server.next())
                .await
                .is_err(),
            "recovery cannot resend"
        );
        server.send(Message::Text(json!({"method":"serverRequest/resolved","params":{"threadId":"thread-a1","requestId":77}}).to_string().into())).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !feed.snapshot().questions[label.as_str()].pending.is_empty() {
                match updates.recv().await {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(error) => panic!("fixture update stream closed: {error}"),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(owners.answer(ask).await.status, 409);
        let initial = feed.snapshot().questions[label.as_str()].owner.clone();
        owners.reconcile(wanted);
        assert_eq!(feed.snapshot().questions[label.as_str()].owner, initial);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), server.next())
                .await
                .is_err()
        );
        let mut cfg = crate::agent_store::load_agent_config(repo.path(), &label)
            .unwrap()
            .unwrap();
        cfg.session.as_mut().unwrap().id = SessionId::parse("replacement").unwrap();
        crate::agent_store::save_agent_config(repo.path(), &label, &cfg).unwrap();
        let changed = target_is_current(
            repo.path(),
            &owners.state.lock().unwrap().lanes[label.as_str()].target,
            None,
        );
        assert!(!changed);
        owners.reconcile(BTreeMap::new());
        assert!(feed.snapshot().questions.is_empty());
        owners.stop();
        owners.join().await;
        let ended = tokio::time::timeout(Duration::from_secs(2), server.next())
            .await
            .unwrap();
        assert!(ended.is_none_or(|frame| frame.is_err()));
        assert!(owners.state.lock().unwrap().tasks.is_empty());
    }
}
