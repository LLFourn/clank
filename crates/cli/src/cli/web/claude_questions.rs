//! Volatile Claude hook ownership. PermissionRequest races the local dialog;
//! only native PostToolUse identifies the winning answer. No idle polling.
use super::{
    Feed,
    native_questions::{Delivery, Request, Window},
    transcript::questions,
};
use anyhow::{Result, ensure};
use clank_core::{Tool, ids::AgentLabel};
use clank_web::request::{BrowserReply, QuestionAnswer};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::oneshot;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Hook {
    pub cwd: String,
    pub session_id: String,
    pub prompt_id: String,
    pub hook_event_name: String,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_use_id: Option<String>,
    pub tool_response: Option<Value>,
}
struct Output {
    value: Value,
    written: oneshot::Sender<bool>,
}
struct Ticket {
    hook: Hook,
    profile: crate::cli::teams_config::AgentDescription,
    label: String,
    owner: String,
    request: Request,
    output: Option<oneshot::Sender<Output>>,
    attempted: Option<blake3::Hash>,
    closed: bool,
    receipted: bool,
}
pub(crate) struct Registry {
    stopped: AtomicBool,
    repo: PathBuf,
    feed: Feed,
    epoch: String,
    tickets: Mutex<BTreeMap<String, Ticket>>,
}
fn key(h: &Hook) -> Option<String> {
    Some(serde_json::to_string(&(&h.session_id, &h.prompt_id, h.tool_use_id.as_ref()?)).ok()?)
}
fn normalized(input: &Value) -> Value {
    let mut value = input.clone();
    if let Some(m) = value.as_object_mut() {
        m.remove("answers");
        // Claude adds response annotations when the terminal answers. These
        // are result metadata, not a change to the registered question input.
        m.remove("annotations");
    }
    value
}
fn answer_hash(request: &Request, answers: &Value) -> Option<blake3::Hash> {
    let answers = answers.as_object()?;
    if answers.len() != request.question.fields.len() {
        return None;
    }
    let canonical: BTreeMap<_, _> = request
        .question
        .fields
        .iter()
        .map(|f| Some((f.question.as_str(), answers.get(&f.question)?.as_str()?)))
        .collect::<Option<_>>()?;
    Some(blake3::hash(&serde_json::to_vec(&canonical).ok()?))
}
fn reply(status: u16, state: &str, message: &str) -> BrowserReply {
    super::native_questions::answer_reply(status, state, message)
}
impl Registry {
    pub(crate) fn new(repo: PathBuf, feed: Feed) -> Arc<Self> {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).expect("Claude question nonce");
        Arc::new(Self {
            stopped: AtomicBool::new(false),
            repo: dunce::canonicalize(&repo).unwrap_or(repo),
            feed,
            epoch: nonce.iter().map(|b| format!("{b:02x}")).collect(),
            tickets: Mutex::new(BTreeMap::new()),
        })
    }
    fn label(&self, session: &str) -> Option<String> {
        let team = crate::agent_store::try_resolve_via_team(&self.repo).ok()??;
        let labels = std::iter::once((&team.master, &team.master_desc))
            .chain(team.members.iter().map(|m| (&m.label, &m.desc)))
            .filter(|(_, desc)| desc.tool == Tool::Claude)
            .map(|(label, _)| label);
        let mut found = labels.filter(|label| {
            crate::agent_store::load_agent_config(&self.repo, label)
                .ok()
                .flatten()
                .and_then(|c| c.session)
                .is_some_and(|s| s.tool == Tool::Claude && s.id.as_str() == session)
        });
        let first = found.next()?.as_str().to_owned();
        found.next().is_none().then_some(first)
    }
    fn description(&self, label: &str) -> Option<crate::cli::teams_config::AgentDescription> {
        let team = crate::agent_store::try_resolve_via_team(&self.repo).ok()??;
        if team.master.as_str() == label {
            Some(team.master_desc)
        } else {
            team.members
                .into_iter()
                .find(|m| m.label.as_str() == label)
                .map(|m| m.desc)
        }
    }
    fn current(&self, t: &Ticket) -> bool {
        self.valid(&t.hook).as_deref() == Some(&t.label)
            && self.description(&t.label).as_ref() == Some(&t.profile)
    }
    fn valid(&self, h: &Hook) -> Option<String> {
        if self.stopped.load(Ordering::Acquire)
            || h.tool_name != "AskUserQuestion"
            || h.session_id.is_empty()
            || h.prompt_id.is_empty()
        {
            return None;
        }
        let repo = crate::cli::resolve_cwd_repo(Path::new(&h.cwd)).ok()?;
        if dunce::canonicalize(repo).ok()? != dunce::canonicalize(&self.repo).ok()? {
            return None;
        }
        self.label(&h.session_id)
    }
    pub(crate) fn owns(&self, agent: &str) -> bool {
        let Ok(label) = AgentLabel::parse(agent) else {
            return false;
        };
        crate::agent_store::load_agent_config(&self.repo, &label)
            .ok()
            .flatten()
            .and_then(|c| c.session)
            .is_some_and(|s| s.tool == Tool::Claude)
    }
    /// Existing repo observation and bridge shutdown are the lifecycle owners.
    pub(crate) fn reconcile(&self) {
        if self
            .tickets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
        {
            return;
        }
        // Existing status observations already discovered the repo. Resolve the
        // roster once and read each binding once; do not rediscover git for
        // every retained question while the provider is idle.
        let mut current = BTreeMap::new();
        if let Some(team) = crate::agent_store::try_resolve_via_team(&self.repo)
            .ok()
            .flatten()
        {
            for (label, desc) in std::iter::once((team.master, team.master_desc))
                .chain(team.members.into_iter().map(|m| (m.label, m.desc)))
            {
                if desc.tool != Tool::Claude {
                    continue;
                }
                if let Some(session) = crate::agent_store::load_agent_config(&self.repo, &label)
                    .ok()
                    .flatten()
                    .and_then(|c| c.session)
                    .filter(|s| s.tool == Tool::Claude)
                {
                    current.insert(
                        label.as_str().to_owned(),
                        (desc, session.id.as_str().to_owned()),
                    );
                }
            }
        }
        let mut counts = BTreeMap::new();
        for (_, session) in current.values() {
            *counts.entry(session.clone()).or_insert(0) += 1;
        }
        current.retain(|_, (_, session)| counts.get(session) == Some(&1));
        let mut tickets = self.tickets.lock().unwrap_or_else(|e| e.into_inner());
        let gone: Vec<_> = tickets
            .iter()
            .filter(|(_, t)| {
                current.get(&t.label).is_none_or(|(desc, session)| {
                    desc != &t.profile || session != &t.hook.session_id
                })
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in gone {
            if let Some(t) = tickets.remove(&k) {
                self.feed.question_release(&t.label, &t.owner);
            }
        }
    }
    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        let mut tickets = self.tickets.lock().unwrap_or_else(|e| e.into_inner());
        for (_, t) in std::mem::take(&mut *tickets) {
            self.feed.question_release(&t.label, &t.owner);
        }
    }
    fn pre(&self, h: Hook) {
        let Some(label) = self.valid(&h) else { return };
        let Some(k) = key(&h) else { return };
        if h.tool_input.get("answers").is_some() {
            return;
        }
        let Some(mut question) =
            questions::parse(Tool::Claude, "AskUserQuestion", &h.tool_input.to_string())
        else {
            return;
        };
        let distinct: std::collections::BTreeSet<_> =
            question.fields.iter().map(|f| &f.question).collect();
        if distinct.len() != question.fields.len() {
            return;
        }
        for (i, f) in question.fields.iter_mut().enumerate() {
            f.id = Some(i.to_string());
            f.is_other = true;
        }
        let Some(profile) = self.description(&label) else {
            return;
        };
        let owner = format!("claude:{}:{}", self.epoch, h.session_id);
        let mut tickets = self.tickets.lock().unwrap_or_else(|e| e.into_inner());
        if self.valid(&h).as_deref() != Some(&label) {
            return;
        }
        // A new prompt retires abandoned pre-registrations and old completion
        // metadata. Exact replay cannot replace an already held hook/attempt.
        tickets.retain(|_, t| {
            t.label != label
                || (t.hook.session_id == h.session_id && t.hook.prompt_id == h.prompt_id)
        });
        if tickets.contains_key(&k) {
            return;
        }
        if self
            .feed
            .question_window(&label)
            .is_some_and(|w| w.owner != owner || w.pending.iter().any(|r| r.turn != h.prompt_id))
        {
            self.feed.question_claim(&label, None);
        }
        let request = Request {
            id: json!(h.tool_use_id.as_ref().unwrap()),
            turn: h.prompt_id.clone(),
            item: h.tool_use_id.clone().unwrap(),
            blocking: true,
            question,
            delivery: Delivery::Pending,
        };
        tickets.insert(
            k,
            Ticket {
                hook: h,
                profile,
                label,
                owner,
                request,
                output: None,
                attempted: None,
                closed: false,
                receipted: false,
            },
        );
    }
    fn permission(&self, h: &Hook) -> Option<(String, oneshot::Receiver<Output>)> {
        let label = self.valid(h)?;
        let mut tickets = self.tickets.lock().unwrap_or_else(|e| e.into_inner());
        if self.valid(h).as_deref() != Some(&label) {
            return None;
        }
        let candidates: Vec<_> = tickets
            .iter()
            .filter(|(_, t)| {
                t.label == label
                    && self.current(t)
                    && t.hook.session_id == h.session_id
                    && t.hook.prompt_id == h.prompt_id
                    && t.hook.tool_input == h.tool_input
                    && !t.closed
            })
            .map(|(k, _)| k.clone())
            .collect();
        if candidates.len() != 1 {
            return None;
        }
        let k = candidates[0].clone();
        let t = tickets.get_mut(&k)?;
        if t.output.is_some() || t.attempted.is_some() {
            return None;
        }
        let (sender, receiver) = oneshot::channel();
        t.output = Some(sender);
        let mut window = self
            .feed
            .question_window(&label)
            .filter(|w| w.owner == t.owner)
            .unwrap_or(Window {
                session: h.session_id.clone(),
                owner: t.owner.clone(),
                available: true,
                reason: None,
                pending: vec![],
                ended: None,
            });
        window.pending.push(t.request.clone());
        self.feed.question_claim(&label, Some(window));
        Some((k, receiver))
    }
    fn cancel(&self, k: &str) {
        let mut tickets = self.tickets.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = tickets.get_mut(k).filter(|t| !t.closed) {
            t.closed = true;
            t.output.take();
            self.feed.question_update(&t.label, &t.owner, |w| {
                if let Some(r) = w.pending.iter_mut().find(|r| r.id == t.request.id) {
                    r.delivery = if t.attempted.is_some() {
                        Delivery::Unknown
                    } else {
                        Delivery::Unavailable
                    };
                }
            });
        }
    }
    fn complete(&self, h: &Hook) {
        let Some(label) = self.valid(h) else { return };
        let Some(k) = key(h) else { return };
        let mut tickets = self.tickets.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t) = tickets.get_mut(&k) else { return };
        if !self.current(t) || t.label != label || t.receipted {
            return;
        }
        t.closed = true;
        t.receipted = true;
        let answers = h.tool_response.as_ref().and_then(|r| r.get("answers"));
        let winner = if normalized(&h.tool_input) == normalized(&t.hook.tool_input) {
            answers.and_then(|a| answer_hash(&t.request, a))
        } else {
            None
        };
        let delivery = if h.hook_event_name == "PostToolUseFailure" {
            Some(Delivery::Failed)
        } else if winner.is_some() && winner == t.attempted {
            Some(Delivery::Answered)
        } else if winner.is_some() {
            Some(Delivery::Local)
        } else {
            None
        };
        self.feed.question_update(&t.label, &t.owner, |w| {
            if let Some(delivery) = delivery {
                if let Some(r) = w.pending.iter_mut().find(|r| r.id == t.request.id) {
                    r.delivery = delivery;
                }
            } else {
                w.pending.retain(|r| r.id != t.request.id);
            }
        });
        // Local completion need not kill PermissionRequest. Release it without
        // returning a remote decision, then keep only the completion metadata.
        if let Some(output) = t.output.take() {
            let (written, _) = oneshot::channel();
            let _ = output.send(Output {
                value: json!({}),
                written,
            });
        }
    }
    pub(crate) async fn answer(&self, ask: QuestionAnswer) -> BrowserReply {
        let (response, label, owner, id) = {
            let mut tickets = self.tickets.lock().unwrap_or_else(|e| e.into_inner());
            let Some(t) = tickets.values_mut().find(|t| {
                t.label == ask.agent
                    && t.owner == ask.owner
                    && t.hook.session_id == ask.session
                    && t.request.id.to_string() == ask.id
                    && t.request.turn == ask.turn
                    && t.request.item == ask.item
            }) else {
                return reply(
                    409,
                    "stale",
                    "The Claude question owner changed. No answer was sent.",
                );
            };
            if t.closed || !self.current(t) {
                return reply(
                    409,
                    "stale",
                    "This native question is no longer pending. No answer was sent.",
                );
            }
            if t.attempted.is_some() {
                return reply(
                    409,
                    "unknown",
                    "An answer was already attempted. Check Terminal before taking further action.",
                );
            }
            if t.output.is_none() {
                return reply(
                    409,
                    "unavailable",
                    "The Claude hook is no longer available. No answer was sent.",
                );
            }
            let payload = match super::native_questions::answer_payload(&t.request, &ask) {
                Ok(p) => p,
                Err(_) => {
                    return reply(
                        400,
                        "invalid",
                        "Supply a valid answer for every field. No answer was sent.",
                    );
                }
            };
            let answers: serde_json::Map<String, Value> = t
                .request
                .question
                .fields
                .iter()
                .map(|f| {
                    let values = payload["result"]["answers"][f.id.as_ref().unwrap()]["answers"]
                        .as_array()
                        .unwrap();
                    (
                        f.question.clone(),
                        json!(
                            values
                                .iter()
                                .map(|v| v.as_str().unwrap())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    )
                })
                .collect();
            let answers = Value::Object(answers);
            t.attempted = answer_hash(&t.request, &answers);
            let Some(output) = t.output.take() else {
                return reply(
                    409,
                    "unavailable",
                    "The Claude hook is no longer available. No answer was sent.",
                );
            };
            let mut updated = t.hook.tool_input.clone();
            updated["answers"] = answers;
            let (written, response) = oneshot::channel();
            let value = json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedInput":updated}}});
            self.feed.question_update(&t.label, &t.owner, |w| {
                if let Some(r) = w.pending.iter_mut().find(|r| r.id == t.request.id) {
                    r.delivery = Delivery::Sending;
                }
            });
            if output.send(Output { value, written }).is_err() {
                return reply(
                    502,
                    "unknown",
                    "The Claude hook stopped. Delivery is unconfirmed; check Terminal.",
                );
            }
            (
                response,
                t.label.clone(),
                t.owner.clone(),
                t.request.id.clone(),
            )
        };
        if response.await.unwrap_or(false) {
            self.feed.question_update(&label, &owner, |w| {
                if let Some(r) = w
                    .pending
                    .iter_mut()
                    .find(|r| r.id == id && r.delivery == Delivery::Sending)
                {
                    r.delivery = Delivery::Submitted;
                }
            });
            reply(
                202,
                "submitted",
                "Answer sent to the native hook; awaiting Claude's completion receipt.",
            )
        } else {
            reply(
                502,
                "unknown",
                "Answer delivery is unconfirmed. Check Terminal before trying again.",
            )
        }
    }
}

/// One broker owned by an existing bridge. The lease survives crashes; the
/// socket is replaced only while holding that lease. No helper process runs.
pub(crate) struct Broker {
    task: Option<tokio::task::JoinHandle<()>>,
    cancel: tokio::sync::watch::Sender<bool>,
    path: PathBuf,
    _lease: std::fs::File,
}
impl Broker {
    pub(crate) async fn stop(mut self) {
        let _ = self.cancel.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
        if let Some(task) = &self.task {
            task.abort();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}
impl Registry {
    pub(crate) fn start(self: &Arc<Self>) -> Result<Broker> {
        use std::os::unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            io::AsRawFd,
        };
        crate::cli::monitor::ensure_runtime_dir().map_err(anyhow::Error::msg)?;
        let path = crate::cli::monitor::runtime_path(&self.repo, "claude-questions.sock");
        let lease = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(crate::cli::monitor::runtime_path(
                &self.repo,
                "claude-questions.lock",
            ))?;
        let meta = lease.metadata()?;
        ensure!(
            meta.is_file() && meta.uid() == unsafe { libc::geteuid() },
            "untrusted Claude question lease"
        );
        ensure!(
            unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "a Claude question broker already owns this repo"
        );
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let listener = tokio::net::UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let registry = self.clone();
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _=cancelled.changed()=>{connections.shutdown().await;break;}
                connection=listener.accept()=>{
                        let Ok((socket,_))=connection else{break};
                        if socket.peer_cred().is_ok_and(|peer|peer.uid()==unsafe{libc::geteuid()}) {
                            let registry=registry.clone();connections.spawn(async move {let _=registry.serve(socket).await;});
                        }
                    }
                    _=connections.join_next(),if !connections.is_empty()=>{}
                }
            }
        });
        Ok(Broker {
            task: Some(task),
            cancel,
            path,
            _lease: lease,
        })
    }
    async fn serve(self: Arc<Self>, socket: tokio::net::UnixStream) -> Result<()> {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let (read, mut write) = socket.into_split();
        let mut read = BufReader::new(read);
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            read.read_line(&mut line),
        )
        .await??;
        let frame: Frame = serde_json::from_str(&line)?;
        ensure!(frame.proto == 1, "unsupported hook protocol");
        let h = frame.input;
        let mut guard = None;
        let mut ack = None;
        let output = match h.hook_event_name.as_str() {
            "PreToolUse" => {
                self.pre(h);
                json!({})
            }
            "PostToolUse" | "PostToolUseFailure" => {
                self.complete(&h);
                json!({})
            }
            "PermissionRequest" => match self.permission(&h) {
                Some((key, receiver)) => {
                    guard = Some(Held {
                        registry: self.clone(),
                        key,
                        armed: true,
                    });
                    tokio::select! {
                        biased;
                        _=read.read_u8()=>return Ok(()),
                        output=receiver=>match output {Ok(output)=>{ack=Some(output.written);output.value},Err(_)=>json!({})}
                    }
                }
                None => json!({}),
            },
            _ => json!({}),
        };
        let mut bytes = serde_json::to_vec(&HookReply { proto: 1, output })?;
        bytes.push(b'\n');
        let sent =
            tokio::time::timeout(std::time::Duration::from_secs(10), write.write_all(&bytes))
                .await
                .is_ok_and(|r| r.is_ok());
        if let Some(ack) = ack {
            let _ = ack.send(sent);
        }
        // A written remote decision waits for PostToolUse. EOF before the
        // decision invalidates the request; a local completion is already closed.
        if sent {
            if let Some(held) = guard.as_mut() {
                held.armed = false;
            }
        }
        drop(guard);
        Ok(())
    }
}
struct Held {
    armed: bool,
    registry: Arc<Registry>,
    key: String,
}
impl Drop for Held {
    fn drop(&mut self) {
        if self.armed {
            self.registry.cancel(&self.key);
        }
    }
}
#[derive(Deserialize, Serialize)]
struct Frame {
    proto: u32,
    input: Hook,
}
#[derive(Deserialize, Serialize)]
struct HookReply {
    proto: u32,
    output: Value,
}

/// Native command adapter. Missing/stale/unsupported brokers immediately leave
/// the ordinary terminal flow alone; this command never starts a broker.
/// Record what this hook says about a bound agent's question, before any
/// broker is asked, so the question is known with no viewer attached.
fn record(hook: &Hook) {
    use crate::cli::question_state::{Key, Observation, input_hash, observe};
    let Ok(repo) = crate::cli::resolve_cwd_repo(Path::new(&hook.cwd)) else {
        return;
    };
    let Ok(session) = clank_core::ids::SessionId::parse(&hook.session_id) else {
        return;
    };
    let Ok(label) = crate::agent_env::resolve_identity_for_hook_in(&repo, Tool::Claude, &session)
    else {
        return;
    };
    let key = |tool_use: &String| Key {
        session: hook.session_id.clone(),
        prompt: hook.prompt_id.clone(),
        tool_use: tool_use.clone(),
    };
    let observation = match (hook.hook_event_name.as_str(), &hook.tool_use_id) {
        ("PreToolUse", Some(id)) => Observation::Asked {
            key: key(id),
            header: header_of(&hook.tool_input),
            input: input_hash(&hook.tool_input),
        },
        ("PostToolUse" | "PostToolUseFailure", Some(id)) => Observation::Result { key: key(id) },
        // PermissionRequest carries no tool call id and changes nothing
        // already recorded; anything without an id cannot be keyed.
        _ => return,
    };
    observe(
        &crate::agent_store::agents_root(&repo).join(label.as_str()),
        [observation],
    );
}

/// One line saying what is asked: the first question's header, or else its
/// first line.
fn header_of(input: &Value) -> String {
    let first = input.pointer("/questions/0");
    let pick = |key: &str| {
        first
            .and_then(|q| q.get(key))
            .and_then(|v| v.as_str())
            .map(|s| s.lines().next().unwrap_or_default().trim().to_string())
            .filter(|s| !s.is_empty())
    };
    pick("header")
        .or_else(|| pick("question"))
        .unwrap_or_else(|| "a question".into())
}

pub(crate) async fn run() -> Result<()> {
    let input = std::io::read_to_string(std::io::stdin())?;
    if let Ok(hook) = serde_json::from_str::<Hook>(&input) {
        record(&hook);
    }
    let result = async {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let hook: Hook = serde_json::from_str(&input)?;
        let repo = crate::cli::resolve_cwd_repo(Path::new(&hook.cwd))?;
        let path = crate::cli::monitor::runtime_path(&repo, "claude-questions.sock");
        let meta = std::fs::symlink_metadata(&path)?;
        ensure!(
            meta.file_type().is_socket() && meta.uid() == unsafe { libc::geteuid() },
            "untrusted question socket"
        );
        let parked = hook.hook_event_name == "PermissionRequest";
        let frame = Frame {
            proto: 1,
            input: hook,
        };
        let mut socket = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::net::UnixStream::connect(path),
        )
        .await??;
        let mut bytes = serde_json::to_vec(&frame)?;
        bytes.push(b'\n');
        tokio::time::timeout(std::time::Duration::from_secs(10), socket.write_all(&bytes))
            .await??;
        let mut reader = BufReader::new(socket);
        let mut line = String::new();
        if parked {
            reader.read_line(&mut line).await?;
        } else {
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                reader.read_line(&mut line),
            )
            .await??;
        }
        let reply: HookReply = serde_json::from_str(&line)?;
        ensure!(reply.proto == 1, "unsupported question reply");
        Ok::<_, anyhow::Error>(reply.output)
    }
    .await;
    println!("{}", result.unwrap_or_else(|_| json!({})));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::teams_config::{AgentDescription, RosterRole};
    use clank_core::ids::SessionId;
    struct ProviderChild(std::process::Child);
    impl Drop for ProviderChild {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                unsafe {
                    libc::kill(self.0.id() as i32, libc::SIGTERM);
                }
            }
            let _ = self.0.wait();
        }
    }
    /// Combined installed-provider proof, using the actual Clank command on
    /// every native hook. No real credentials, production profile or inference.
    #[tokio::test]
    #[ignore = "requires installed Claude and a freshly built Clank binary"]
    async fn installed_claude_uses_the_real_clank_hook_and_broker_end_to_end() {
        use std::io::Write;
        use tokio::io::{AsyncBufReadExt, BufReader};
        let claude =
            std::env::var_os("CLANK_CLAUDE_PROTOCOL_BINARY").expect("select installed Claude");
        let clank = std::env::var_os("CLANK_QUESTION_BINARY").expect("select freshly built Clank");
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/it/fixtures/claude_questions.py");
        for scenario in ["remote", "local", "cancel"] {
            let (repo, registry, _) = fixture();
            let session = "9129c0e1-8f66-4168-b3c2-a5b831c69a13";
            crate::agent_store::bind_session_to_agent(
                repo.path(),
                &AgentLabel::parse("claude-question-agent").unwrap(),
                Tool::Claude,
                &SessionId::parse(session).unwrap(),
            )
            .unwrap();
            let broker = registry.start().unwrap();
            let mut child = ProviderChild(
                std::process::Command::new("python3")
                    .arg(&script)
                    .arg(&claude)
                    .args(["--broker", scenario])
                    .arg(repo.path())
                    .arg(&clank)
                    .arg(session)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::inherit())
                    .spawn()
                    .unwrap(),
            );
            let stdout =
                std::fs::File::from(std::os::fd::OwnedFd::from(child.0.stdout.take().unwrap()));
            let mut reader = BufReader::new(tokio::fs::File::from_std(stdout));
            let mut line = String::new();
            tokio::time::timeout(
                std::time::Duration::from_secs(40),
                reader.read_line(&mut line),
            )
            .await
            .unwrap()
            .unwrap();
            let ready: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(ready["ready"], true);
            let agent_dir =
                crate::agent_store::agents_root(repo.path()).join("claude-question-agent");
            let asking =
                || crate::cli::question_state::asking(&agent_dir, session).map(|q| q.header);
            assert_eq!(asking(), Some("Direction".into()), "{scenario}: recorded");
            let (_, mut updates) = registry.feed.bridge_connect();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while !registry
                    .feed
                    .question_window("claude-question-agent")
                    .is_some_and(|w| !w.pending.is_empty())
                {
                    let _ = updates.recv().await;
                }
            })
            .await
            .unwrap();
            let window = registry
                .feed
                .question_window("claude-question-agent")
                .unwrap();
            let request = &window.pending[0];
            assert_eq!(request.item, "toolu_clank_question_proof");
            assert_eq!(request.turn, ready["input"]["prompt_id"]);
            let ask = QuestionAnswer {
                agent: "claude-question-agent".into(),
                session: session.into(),
                owner: window.owner,
                id: request.id.to_string(),
                turn: request.turn.clone(),
                item: request.item.clone(),
                attempt: "installed-proof".into(),
                answers: BTreeMap::from([("0".into(), vec!["South".into()])]),
            };
            if scenario == "remote" {
                assert_eq!(registry.answer(ask.clone()).await.status, 202);
            }
            child
                .0
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"continue\n")
                .unwrap();
            line.clear();
            tokio::time::timeout(
                std::time::Duration::from_secs(40),
                reader.read_line(&mut line),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                line.contains(&format!("PASS: interactive Claude {scenario};")),
                "{line}"
            );
            let expected = match scenario {
                "remote" => Delivery::Answered,
                "local" => Delivery::Local,
                _ => Delivery::Unavailable,
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if registry
                        .feed
                        .question_window("claude-question-agent")
                        .is_some_and(|w| w.pending.first().is_some_and(|r| r.delivery == expected))
                    {
                        break;
                    }
                    let _ = updates.recv().await;
                }
            })
            .await
            .unwrap();
            if scenario == "cancel" {
                // No result hook and no Stop: only the record says so.
                assert!(asking().is_some(), "a cancel runs no hook");
                let binding: clank_core::agent_config::Session = serde_json::from_value(json!({
                    "id": session,
                    "tool": "claude",
                    "updated_at": "2026-10-06T00:00:00Z",
                    "transcript": ready["input"]["transcript_path"],
                }))
                .unwrap();
                crate::cli::recovery::Driver::new(repo.path().into(), "/nonexistent-home".into())
                    .reconcile(
                        vec![crate::cli::recovery::Facts {
                            author: AgentLabel::parse("claude-question-agent").unwrap(),
                            agent_dir: agent_dir.clone(),
                            session: binding,
                            auto: true,
                            delivering: false,
                            obligated: false,
                            role: "master".into(),
                            work: Vec::new(),
                            blind_nudge: None,
                            attending: false,
                            asking: asking().is_some(),
                            identify: false,
                            projection: Default::default(),
                        }],
                        0,
                    );
            }
            assert_eq!(asking(), None, "{scenario}: ended");
            assert_eq!(
                registry.answer(ask).await.status,
                409,
                "a completed/cancelled question cannot send again"
            );
            assert!(child.0.try_wait().unwrap().is_none());
            child
                .0
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"finish\n")
                .unwrap();
            assert!(child.0.wait().unwrap().success());
            registry.stop();
            broker.stop().await;
            std::fs::remove_file(crate::cli::monitor::runtime_path(
                &registry.repo,
                "claude-questions.lock",
            ))
            .unwrap();
        }
    }

    fn fixture() -> (tempfile::TempDir, Arc<Registry>, Hook) {
        let repo = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
        let label = AgentLabel::parse("claude-question-agent").unwrap();
        crate::cli::agent::add_repo_roster_agent(
            repo.path(),
            &label,
            AgentDescription {
                tool: Tool::Claude,
                launch: None,
                initial_prompt: None,
            },
            RosterRole::Master,
        )
        .unwrap();
        crate::agent_store::bind_session_to_agent(
            repo.path(),
            &label,
            Tool::Claude,
            &SessionId::parse("claude-fixture").unwrap(),
        )
        .unwrap();
        let registry = Registry::new(repo.path().into(), Feed::new());
        let hook = Hook {
            cwd: repo.path().to_str().unwrap().into(),
            session_id: "claude-fixture".into(),
            prompt_id: "prompt".into(),
            hook_event_name: "PreToolUse".into(),
            tool_name: "AskUserQuestion".into(),
            tool_use_id: Some("tool-native".into()),
            tool_response: None,
            tool_input: json!({"questions":[{"question":"Choose direction","header":"Direction","options":[{"label":"North","description":"first"},{"label":"South","description":"second"}],"multiSelect":false}],"metadata":{"nativeExtra":42}}),
        };
        (repo, registry, hook)
    }
    fn ask(registry: &Registry, hook: &Hook) -> QuestionAnswer {
        let w = registry
            .feed
            .question_window("claude-question-agent")
            .unwrap();
        QuestionAnswer {
            agent: "claude-question-agent".into(),
            session: hook.session_id.clone(),
            owner: w.owner,
            id: json!(hook.tool_use_id).to_string(),
            turn: hook.prompt_id.clone(),
            item: hook.tool_use_id.clone().unwrap(),
            attempt: "browser-attempt".into(),
            answers: BTreeMap::from([("0".into(), vec!["South".into()])]),
        }
    }
    fn post(hook: &Hook, answer: &str) -> Hook {
        let mut h = hook.clone();
        h.hook_event_name = "PostToolUse".into();
        h.tool_response = Some(json!({"answers":{"Choose direction":answer}}));
        h
    }
    fn record_in(hook: &Hook) {
        record(hook);
    }

    /// With no broker listening, the hook still records a bound agent's
    /// question, and its own result ends it. Another session's hooks, and
    /// a PermissionRequest (no tool call id), change nothing.
    #[test]
    fn the_hook_records_a_bound_agents_question_without_a_broker() {
        use crate::cli::question_state::asking;
        let (repo, _registry, hook) = fixture();
        let dir = crate::agent_store::agents_root(repo.path()).join("claude-question-agent");
        record_in(&hook);
        assert_eq!(
            asking(&dir, "claude-fixture").map(|q| q.header),
            Some("Direction".into())
        );
        let mut permission = hook.clone();
        permission.hook_event_name = "PermissionRequest".into();
        permission.tool_use_id = None;
        record_in(&permission);
        assert!(asking(&dir, "claude-fixture").is_some());
        let mut foreign = post(&hook, "South");
        foreign.session_id = "another-session".into();
        record_in(&foreign);
        assert!(
            asking(&dir, "claude-fixture").is_some(),
            "another session's result"
        );
        // An old session of this very agent is not the bound one.
        let mut stale = hook.clone();
        stale.session_id = "old-session".into();
        stale.tool_use_id = Some("tool-old".into());
        record(&stale);
        assert_eq!(
            asking(&dir, "old-session"),
            None,
            "an unbound session's question"
        );
        record_in(&post(&hook, "South"));
        assert_eq!(asking(&dir, "claude-fixture"), None);
        record_in(&hook);
        assert_eq!(
            asking(&dir, "claude-fixture"),
            None,
            "a replayed pre-tool hook"
        );
        let mut failing = hook.clone();
        failing.tool_use_id = Some("tool-two".into());
        record_in(&failing);
        failing.hook_event_name = "PostToolUseFailure".into();
        record_in(&failing);
        assert_eq!(asking(&dir, "claude-fixture"), None);
    }

    /// What the hook records is what the remote status frame says, to a
    /// viewer who connects at any time after: the frame is derived from
    /// what the monitor publishes of the record at its next fold, with no
    /// broker or viewer present when it was asked.
    #[tokio::test]
    async fn the_status_frame_says_a_recorded_question_is_waiting() {
        let (repo, _registry, hook) = fixture();
        let frame = || async {
            crate::cli::monitor::publish_one_fold(repo.path()).await;
            let snap = crate::cli::status::snapshot(repo.path(), None)
                .await
                .unwrap();
            crate::cli::web::status_payload(repo.path(), &snap, &Default::default())
                ["facts"]["agents"][0]["asking"]
                .clone()
        };
        assert_eq!(frame().await, Value::Null);
        record_in(&hook);
        assert_eq!(frame().await, json!("Direction"));
        record_in(&post(&hook, "South"));
        assert_eq!(frame().await, Value::Null);
    }

    #[tokio::test]
    async fn remote_receipt_and_local_winner_are_native_and_never_replayed_as_plaintext() {
        for local in [false, true] {
            let (_repo, registry, hook) = fixture();
            registry.pre(hook.clone());
            let (_, receiver) = registry.permission(&hook).unwrap();
            let mut answer = ask(&registry, &hook);
            let remote = if local {
                "South"
            } else {
                "private-fixture-value"
            };
            answer.answers.insert("0".into(), vec![remote.into()]);
            let (reply, output) = tokio::join!(registry.answer(answer.clone()), async {
                let output = receiver.await.unwrap();
                let value = output.value;
                output.written.send(true).unwrap();
                value
            });
            assert_eq!(reply.status, 202);
            let updated = &output["hookSpecificOutput"]["decision"]["updatedInput"];
            assert_eq!(updated["metadata"], hook.tool_input["metadata"]);
            assert_eq!(updated["questions"], hook.tool_input["questions"]);
            assert_eq!(updated["answers"]["Choose direction"], remote);
            assert_eq!(registry.answer(answer.clone()).await.status, 409);
            registry.complete(&post(&hook, if local { "North" } else { remote }));
            let w = registry
                .feed
                .question_window("claude-question-agent")
                .unwrap();
            assert_eq!(
                w.pending[0].delivery,
                if local {
                    Delivery::Local
                } else {
                    Delivery::Answered
                }
            );
            assert!(
                !serde_json::to_string(&w)
                    .unwrap()
                    .contains("private-fixture-value")
            );
            assert!(serde_json::to_value(&w).unwrap().get("answers").is_none());
            assert_eq!(registry.answer(answer).await.status, 409);
        }
    }
    #[tokio::test]
    async fn local_completion_releases_a_parked_hook_and_pre_replay_cannot_reopen_it() {
        let (_repo, registry, hook) = fixture();
        registry.pre(hook.clone());
        let (_, receiver) = registry.permission(&hook).unwrap();
        let mut receipt = post(&hook, "North");
        receipt.tool_input["annotations"] = json!({});
        registry.complete(&receipt);
        assert_eq!(receiver.await.unwrap().value, json!({}));
        registry.pre(hook.clone());
        assert!(registry.permission(&hook).is_none());
        assert_eq!(
            registry
                .feed
                .question_window("claude-question-agent")
                .unwrap()
                .pending[0]
                .delivery,
            Delivery::Local
        );
    }
    #[tokio::test]
    async fn response_annotations_do_not_mask_changed_question_input() {
        let (_repo, registry, hook) = fixture();
        registry.pre(hook.clone());
        let (_, receiver) = registry.permission(&hook).unwrap();
        let mut receipt = post(&hook, "North");
        receipt.tool_input["annotations"] = json!({});
        receipt.tool_input["questions"][0]["question"] = json!("Different question");
        registry.complete(&receipt);
        assert_eq!(receiver.await.unwrap().value, json!({}));
        assert!(
            registry
                .feed
                .question_window("claude-question-agent")
                .unwrap()
                .pending
                .is_empty()
        );
        assert_eq!(registry.answer(ask(&registry, &hook)).await.status, 409);
    }
    #[tokio::test]
    async fn cancellation_duplicate_text_ambiguity_and_new_prompt_retire_authority() {
        let (_repo, registry, hook) = fixture();
        registry.pre(hook.clone());
        let (k, receiver) = registry.permission(&hook).unwrap();
        registry.cancel(&k);
        assert!(receiver.await.is_err());
        assert_eq!(
            registry
                .feed
                .question_window("claude-question-agent")
                .unwrap()
                .pending[0]
                .delivery,
            Delivery::Unavailable
        );
        registry.pre(hook.clone());
        assert!(
            registry.permission(&hook).is_none(),
            "EOF cannot recreate answer authority"
        );
        let mut duplicate = hook.clone();
        duplicate.tool_input["questions"]
            .as_array_mut()
            .unwrap()
            .push(hook.tool_input["questions"][0].clone());
        registry.pre(duplicate.clone());
        assert!(registry.permission(&duplicate).is_none());
        let mut hook = hook.clone();
        hook.prompt_id = "parallel".into();
        registry.pre(hook.clone());
        let mut parallel = hook.clone();
        parallel.tool_use_id = Some("other-tool".into());
        registry.pre(parallel);
        assert!(
            registry.permission(&hook).is_none(),
            "cannot invent an id on PermissionRequest"
        );
        let mut next = hook.clone();
        next.prompt_id = "next".into();
        registry.pre(next.clone());
        assert!(registry.permission(&hook).is_none());
        assert!(registry.permission(&next).is_some());
    }
    #[tokio::test]
    async fn clear_and_stop_release_channels_and_do_not_clear_a_replacement_owner() {
        let (repo, registry, hook) = fixture();
        registry.pre(hook.clone());
        let (_, receiver) = registry.permission(&hook).unwrap();
        let label = AgentLabel::parse("claude-question-agent").unwrap();
        crate::agent_store::update_agent_config(repo.path(), &label, |config| {
            config.session = None
        })
        .unwrap();
        registry.feed.question_claim(
            label.as_str(),
            Some(Window {
                session: "replacement".into(),
                owner: "replacement-owner".into(),
                available: true,
                reason: None,
                pending: vec![],
                ended: None,
            }),
        );
        registry.reconcile();
        assert!(receiver.await.is_err());
        assert_eq!(
            registry.feed.question_window(label.as_str()).unwrap().owner,
            "replacement-owner"
        );
        registry.stop();
        registry.pre(hook.clone());
        assert!(registry.tickets.lock().unwrap().is_empty());
    }
    #[test]
    fn an_old_generation_cannot_invalidate_a_replacement_provider_window() {
        let feed = Feed::new();
        feed.question_claim(
            "agent",
            Some(Window {
                session: "session".into(),
                owner: "new-claude".into(),
                available: true,
                reason: None,
                pending: vec![],
                ended: None,
            }),
        );
        feed.question_release("agent", "old-codex");
        assert_eq!(feed.question_window("agent").unwrap().owner, "new-claude");
        feed.question_release("agent", "new-claude");
        assert!(feed.question_window("agent").is_none());
    }

    #[tokio::test]
    async fn broker_lease_is_single_owned_and_shutdown_reaps_parked_connections() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (_repo, registry, mut hook) = fixture();
        let broker = registry.start().unwrap();
        assert!(registry.start().is_err());
        registry.pre(hook.clone());
        hook.hook_event_name = "PermissionRequest".into();
        hook.tool_use_id = None;
        let path = crate::cli::monitor::runtime_path(&registry.repo, "claude-questions.sock");
        let mut client = tokio::net::UnixStream::connect(&path).await.unwrap();
        let mut frame = serde_json::to_vec(&Frame {
            proto: 1,
            input: hook,
        })
        .unwrap();
        frame.push(b'\n');
        client.write_all(&frame).await.unwrap();
        let (_, mut updates) = registry.feed.bridge_connect();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while registry
                .feed
                .question_window("claude-question-agent")
                .is_none()
            {
                let _ = updates.recv().await;
            }
        })
        .await
        .unwrap();
        registry.stop();
        broker.stop().await;
        assert!(!path.exists());
        let mut line = String::new();
        let _ = BufReader::new(client).read_line(&mut line).await;
        assert!(registry.tickets.lock().unwrap().is_empty());
        // This fixture alone owns this now-released lease; production never unlinks locks.
        std::fs::remove_file(crate::cli::monitor::runtime_path(
            &registry.repo,
            "claude-questions.lock",
        ))
        .unwrap();
    }

    #[tokio::test]
    async fn production_socket_handler_keeps_a_written_attempt_until_native_receipt() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (_repo, registry, hook) = fixture();
        registry.pre(hook.clone());
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let r = registry.clone();
        let handler = tokio::spawn(async move { r.serve(server).await.unwrap() });
        let mut permission = hook.clone();
        permission.hook_event_name = "PermissionRequest".into();
        permission.tool_use_id = None;
        let mut frame = serde_json::to_vec(&Frame {
            proto: 1,
            input: permission,
        })
        .unwrap();
        frame.push(b'\n');
        client.write_all(&frame).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if registry
                    .feed
                    .question_window("claude-question-agent")
                    .is_some()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let answer = ask(&registry, &hook);
        let (reply, value) = tokio::join!(registry.answer(answer.clone()), async {
            let mut read = BufReader::new(client);
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            serde_json::from_str::<HookReply>(&line).unwrap().output
        });
        assert_eq!(reply.status, 202);
        assert_eq!(
            value["hookSpecificOutput"]["decision"]["updatedInput"]["answers"]["Choose direction"],
            "South"
        );
        handler.await.unwrap();
        assert_eq!(
            registry
                .feed
                .question_window("claude-question-agent")
                .unwrap()
                .pending[0]
                .delivery,
            Delivery::Submitted
        );
        registry.complete(&post(&hook, "South"));
        assert_eq!(
            registry
                .feed
                .question_window("claude-question-agent")
                .unwrap()
                .pending[0]
                .delivery,
            Delivery::Answered
        );
    }
}
