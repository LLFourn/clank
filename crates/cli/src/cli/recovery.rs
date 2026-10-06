//! Recovering an agent whose turn ended on a provider error.
//!
//! The harnesses run no Stop hook after an API-error turn, and Stop is
//! what re-arms clank's waker, so such an agent keeps its pending work and
//! nothing will ever deliver it again. This module is the decision half of
//! the fix:
//!
//! - which failures are worth retrying;
//! - how each harness's own ordered record is read for turn starts and
//!   ends, since hook timing cannot order them;
//! - the one durable episode per agent that bounds and attributes retries.
//!
//! The monitor owns the side effects (`monitor.rs`). The full contract is
//! the research plan `research-agent-stalls-and-provider-capacity-recovery`.

use std::path::{Path, PathBuf};

use clank_core::Tool;

/// The waits before each retry, in seconds; empty means escalate at once.
///
/// Classes are matched as each harness spells them. A class nobody listed
/// is treated as `unknown`: one late retry, then a person.
pub(crate) fn retries(class: &str) -> &'static [u64] {
    match class {
        "overloaded" | "server_overloaded" | "server_error" | "rate_limit" => &[60, 300, 900],
        "max_output_tokens" => &[60],
        "usage_limit_exceeded"
        | "authentication_failed"
        | "unauthorized"
        | "billing_error"
        | "account_on_hold"
        | "oauth_org_not_allowed"
        | "cloud_credential_error"
        | "model_not_found"
        | "invalid_request"
        | "cyber_policy"
        | "misalignment_policy_violation" => &[],
        _ => &[300],
    }
}

/// OpenCode's error, in the classes [`retries`] knows. It names errors by
/// type, and an `APIError` by its HTTP status.
pub(crate) fn opencode_class(name: &str, status: Option<u16>) -> &'static str {
    match (name, status) {
        ("ProviderAuthError", _) => "authentication_failed",
        ("MessageOutputLengthError", _) => "max_output_tokens",
        ("APIError", Some(429)) => "rate_limit",
        ("APIError", Some(529 | 503)) => "overloaded",
        ("APIError", Some(500..=599)) => "server_error",
        ("APIError", Some(401 | 403)) => "authentication_failed",
        ("APIError", Some(400 | 404 | 413 | 422)) => "invalid_request",
        _ => "unknown",
    }
}

/// One fact from a harness's ordered record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fact {
    /// A turn began. `id` is the harness's own identity for it: Codex's
    /// `turn_id`, Claude's `promptId`, Grok's `turn_number`, OpenCode's user
    /// message id. `text` is the prompt, where the record keeps it.
    Started { id: String, text: Option<String> },
    /// A turn ended. `id` is `None` where the record does not say which
    /// turn, and then it is the latest one started.
    Ended {
        id: Option<String>,
        outcome: Outcome,
    },
    /// The harness is re-running the latest turn by itself.
    NativeRetry,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", content = "class", rename_all = "snake_case")]
pub(crate) enum Outcome {
    Running,
    Completed,
    Failed(String),
    Interrupted,
}

/// What one turn is, as far as the record has said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnRec {
    pub(crate) id: String,
    pub(crate) text: Option<String>,
    pub(crate) outcome: Outcome,
    /// The harness has started re-running it by itself.
    pub(crate) retrying: bool,
}

/// The recent turns of one session, in order: the current-turn state every
/// recovery decision is made from.
///
/// Bounded: a decision needs the latest turn and the turns since an
/// attempt was claimed, never the whole history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    turns: std::collections::VecDeque<TurnRec>,
    /// An end was read after the latest start, for a turn whose start was
    /// not read: the record was entered mid-turn, and the latest turn is
    /// not here.
    unanchored: bool,
}

const SNAPSHOT_TURNS: usize = 64;

impl Snapshot {
    pub(crate) fn apply(&mut self, fact: Fact) {
        match fact {
            Fact::Started { id, text } => {
                if self.latest().is_some_and(|t| t.id == id) {
                    return;
                }
                if self.turns.len() == SNAPSHOT_TURNS {
                    self.turns.pop_front();
                }
                self.unanchored = false;
                self.turns.push_back(TurnRec {
                    id,
                    text,
                    outcome: Outcome::Running,
                    retrying: false,
                });
            }
            Fact::Ended { id, outcome } => {
                let turn = match &id {
                    Some(id) => self.turns.iter_mut().rev().find(|t| &t.id == id),
                    None => self.turns.back_mut(),
                };
                match turn {
                    Some(turn) => {
                        turn.outcome = outcome;
                        turn.retrying = false;
                    }
                    None => self.unanchored = true,
                }
            }
            Fact::NativeRetry => {
                if let Some(turn) = self.turns.back_mut() {
                    turn.retrying = true;
                }
            }
        }
    }

    pub(crate) fn latest(&self) -> Option<&TurnRec> {
        self.turns.back()
    }

    /// The turns' ids, oldest first.
    pub(crate) fn ids(&self) -> impl Iterator<Item = &str> {
        self.turns.iter().map(|t| t.id.as_str())
    }

    /// The turns that began after `id`. When `id` is not here, every turn
    /// here began after it: what is read is always a suffix of the record.
    fn after(&self, id: &str) -> Vec<&TurnRec> {
        let from = self
            .turns
            .iter()
            .position(|t| t.id == id)
            .map_or(0, |i| i + 1);
        self.turns.iter().skip(from).collect()
    }

    fn find(&self, id: &str) -> Option<&TurnRec> {
        self.turns.iter().rev().find(|t| t.id == id)
    }
}

/// Reads one session's record a line at a time, in order.
#[derive(Debug)]
pub(crate) struct RecordReader {
    tool: Tool,
    /// The bound session, for a record that interleaves several.
    session: String,
    current: Option<String>,
}

impl RecordReader {
    pub(crate) fn new(tool: Tool, session: &str) -> Self {
        Self {
            tool,
            session: session.to_string(),
            current: None,
        }
    }

    pub(crate) fn read(&mut self, line: &str) -> Option<Fact> {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let fact = match self.tool {
            Tool::Codex => codex(&v),
            Tool::Claude => claude(&v, self.current.as_deref()),
            Tool::Grok => grok(&v),
            Tool::OpenCode => observed(&v, &self.session),
        }?;
        if let Fact::Started { id, .. } = &fact {
            self.current = Some(id.clone());
        }
        Some(fact)
    }
}

fn string(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|s| s.as_str()).map(str::to_string)
}

/// Codex's rollout. Only `event_msg` records, which Codex writes itself;
/// a `response_item` carrying the same words is something somebody said.
fn codex(v: &serde_json::Value) -> Option<Fact> {
    if v.get("type")?.as_str()? != "event_msg" {
        return None;
    }
    let p = v.get("payload")?;
    let id = string(p, "turn_id")?;
    match p.get("type")?.as_str()? {
        "task_started" => Some(Fact::Started { id, text: None }),
        "task_complete" => Some(Fact::Ended {
            id: Some(id),
            outcome: match p.get("error").filter(|e| !e.is_null()) {
                Some(e) => Outcome::Failed(
                    string(e, "codex_error_info").unwrap_or_else(|| "unknown".into()),
                ),
                None => Outcome::Completed,
            },
        }),
        "turn_aborted" => Some(Fact::Ended {
            id: Some(id),
            outcome: Outcome::Interrupted,
        }),
        _ => None,
    }
}

/// Claude's transcript. A new turn is a user record whose `promptId` has
/// changed; a tool result shares its turn's id. A failure names no turn.
fn claude(v: &serde_json::Value, current: Option<&str>) -> Option<Fact> {
    match v.get("type")?.as_str()? {
        "user" => {
            let id = string(v, "promptId")?;
            if Some(id.as_str()) == current {
                return None;
            }
            let text = match v.get("message")?.get("content")? {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Array(blocks) => Some(
                    blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            };
            Some(Fact::Started { id, text })
        }
        "assistant" if v.get("isApiErrorMessage").and_then(|b| b.as_bool()) == Some(true) => {
            Some(Fact::Ended {
                id: None,
                outcome: Outcome::Failed(string(v, "error").unwrap_or_else(|| "unknown".into())),
            })
        }
        _ => None,
    }
}

/// Grok's `events.jsonl`. Its `turn_ended` carries no class and no turn:
/// the class is `unknown` rather than borrowed from a hook report that may
/// belong to another turn.
fn grok(v: &serde_json::Value) -> Option<Fact> {
    match v.get("type")?.as_str()? {
        "turn_started" => Some(Fact::Started {
            id: v.get("turn_number")?.to_string(),
            text: None,
        }),
        "turn_ended" => Some(Fact::Ended {
            id: None,
            outcome: match v.get("outcome")?.as_str()? {
                "completed" => Outcome::Completed,
                "error" => Outcome::Failed("unknown".into()),
                _ => Outcome::Interrupted,
            },
        }),
        _ => None,
    }
}

/// What clank's OpenCode plugin reported through `clank stop-hook
/// --observe`, one per line in [`observed_path`]. Lines for any other
/// session are not this session's turns, and an end that names no turn is
/// nobody's: it is never attached to whichever turn came last.
fn observed(v: &serde_json::Value, session: &str) -> Option<Fact> {
    if v.get("session")?.as_str()? != session {
        return None;
    }
    let turn = v.get("turn")?.as_str()?;
    if turn == "retry" {
        return Some(Fact::NativeRetry);
    }
    let id = Some(string(v, "id")?);
    match turn {
        "started" => Some(Fact::Started {
            id: id?,
            text: None,
        }),
        "completed" => Some(Fact::Ended {
            id,
            outcome: Outcome::Completed,
        }),
        "failed" => Some(Fact::Ended {
            id,
            outcome: Outcome::Failed(string(v, "class").unwrap_or_else(|| "unknown".into())),
        }),
        "interrupted" => Some(Fact::Ended {
            id,
            outcome: Outcome::Interrupted,
        }),
        _ => None,
    }
}

/// Where a bound session's turns are recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    /// This file, whether or not it exists yet.
    At(PathBuf),
    /// A Codex rollout not written yet: the one that appears under `root`
    /// named for the session.
    Find { root: PathBuf },
}

/// Where a harness keeps the record its turns are read from, for a bound
/// session. `None` when there is no record clank can read.
pub(crate) fn record_source(
    agent_dir: &Path,
    session: &clank_core::agent_config::Session,
    cwd: &Path,
    home: &Path,
) -> Option<Source> {
    use crate::cli::web::transcript::transcript_path;
    let recorded = session.transcript.as_deref().map(Path::new);
    let id = session.id.as_str();
    match session.tool {
        Tool::OpenCode => Some(Source::At(observed_path(agent_dir))),
        Tool::Grok => transcript_path(Tool::Grok, id, recorded, cwd, home)
            .map(|history| Source::At(history.with_file_name("events.jsonl"))),
        Tool::Codex => Some(
            transcript_path(Tool::Codex, id, recorded, cwd, home).map_or_else(
                || Source::Find {
                    root: home.join(".codex").join("sessions"),
                },
                Source::At,
            ),
        ),
        tool => transcript_path(tool, id, recorded, cwd, home).map(Source::At),
    }
}

impl Source {
    /// What to watch for this record to change, appear or be replaced.
    ///
    /// The nearest directory that exists, so creation of each missing level
    /// is seen; non-recursive except for a rollout still to be found,
    /// whose dated directory is not known in advance.
    fn watch(&self) -> WatchKey {
        let (dir, recursive) = match self {
            Source::At(path) => (path.parent().unwrap_or(path), false),
            Source::Find { root } => (root.as_path(), true),
        };
        let existing = dir.ancestors().find(|d| d.is_dir()).unwrap_or(dir);
        WatchKey {
            recursive: recursive && existing == dir,
            dir: existing.to_path_buf(),
        }
    }

    /// The record `path` is, if it is this source's.
    fn found(&self, path: &Path, session: &str) -> Option<PathBuf> {
        match self {
            Source::At(at) => (at == path).then(|| at.clone()),
            Source::Find { root } => (path.starts_with(root)
                && path.extension().is_some_and(|e| e == "jsonl")
                && path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().contains(session)))
            .then(|| path.to_path_buf()),
        }
    }
}

/// The record clank keeps for a harness that reports its own turns.
pub(crate) fn observed_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("turns.jsonl")
}

/// The windows a record is read back over at arm time, widening until the
/// latest turn's start is in view.
const BOOTSTRAP_WINDOWS: [u64; 4] = [256 * 1024, 4 * 1024 * 1024, 64 * 1024 * 1024, u64::MAX];

/// Read a record's recent turns, and keep a tail positioned at its end.
pub(crate) fn bootstrap(
    path: &Path,
    tool: Tool,
    session: &str,
) -> std::io::Result<(crate::cli::web::transcript::Tail, RecordReader, Snapshot)> {
    let mut last = None;
    for window in BOOTSTRAP_WINDOWS {
        let (tail, lines) = crate::cli::web::transcript::Tail::open(path, window)?;
        let mut reader = RecordReader::new(tool, session);
        let mut snap = Snapshot::default();
        for line in &lines {
            if let Some(fact) = reader.read(&line.text) {
                snap.apply(fact);
            }
        }
        let whole = lines.first().is_none_or(|l| l.offset == 0);
        if whole {
            // Nothing earlier to find.
            snap.unanchored = false;
            return Ok((tail, reader, snap));
        }
        if snap.latest().is_some() && !snap.unanchored {
            return Ok((tail, reader, snap));
        }
        last = Some((tail, reader, snap));
    }
    Ok(last.expect("the widest window reads the whole record"))
}

/// How an attempt reaches the agent, where clank can do it safely.
// `CodexStart` is built only once its capability check passes and
// `transport_for` returns it; until then only tests construct it.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    /// Codex's app-server `turn/start`, only after reading that the latest
    /// turn is the failed one.
    CodexStart,
    /// A recovery helper parked on the monitor, which hands the wake to the
    /// harness and answers through [`reply_path`].
    Helper,
}

impl Transport {
    fn name(self) -> &'static str {
        match self {
            Transport::CodexStart => "codex-start",
            Transport::Helper => "helper",
        }
    }
}

/// The transport clank may use for `tool`, if any.
///
/// Empty until each harness's live capability check passes: whether a
/// start on a busy session queues or cancels is unknown for every harness,
/// and recovery must never cancel a turn. An agent with no transport is
/// told about its failure and waits for a person.
pub(crate) fn transport_for(tool: Tool) -> Option<Transport> {
    #[cfg(test)]
    if let Some(t) = tests::enabled(tool) {
        return t;
    }
    let _ = tool;
    None
}

/// Every turn clank has asked for in an episode.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Attempt {
    pub(crate) n: u32,
    pub(crate) claimed_at: u64,
    pub(crate) transport: String,
    /// The failed turn it retries.
    pub(crate) target: String,
    /// What the attempt submits, so its turn is recognisable: an OpenCode
    /// message id is this tag, and a wake's text carries it.
    pub(crate) tag: String,
    /// The turn the attempt started, once the transport or the record says.
    pub(crate) started: Option<String>,
    pub(crate) disposition: Disposition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Disposition {
    Claimed,
    /// Accepted, not yet seen starting.
    Submitted,
    Started,
    Refused,
    /// Never reached the harness. Not an attempt; counted only so an absent
    /// transport cannot keep an episode waiting forever.
    Unavailable,
    /// May or may not have reached it. Never replayed.
    Indeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    Waiting,
    Dispatched,
    Running,
    /// Waiting for a person: latched until a reset, never re-opened by a
    /// duplicate report, a reconnect or a restart.
    Escalated,
    /// Over. Kept so the same failed turn is never opened again.
    Closed,
}

/// One failed turn's recovery, persisted in [`episode_path`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Episode {
    pub(crate) session: String,
    /// The failed turn that opened it: its identity.
    pub(crate) opened_by: String,
    /// The failed turn a retry targets now: the opener, or the latest
    /// attempt's own failed turn.
    pub(crate) failed_turn: String,
    /// The class of `failed_turn`'s failure.
    pub(crate) class: String,
    pub(crate) attempts: Vec<Attempt>,
    /// Unix seconds: the next retry, an answer's deadline, or a submitted
    /// attempt's confirmation bound.
    pub(crate) next_at: Option<u64>,
    pub(crate) state: State,
    /// What the agent was told when this episode escalated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) notice: Option<String>,
    /// Deadlines at which the record could not be read.
    #[serde(default)]
    pub(crate) unobserved: u32,
}

impl Episode {
    fn spent(&self) -> usize {
        self.attempts
            .iter()
            .filter(|a| a.disposition != Disposition::Unavailable)
            .count()
    }

    fn unavailable(&self) -> usize {
        self.attempts
            .iter()
            .filter(|a| a.disposition == Disposition::Unavailable)
            .count()
    }

    fn live(&self) -> bool {
        !matches!(self.state, State::Closed)
    }

    /// The turn after which anything new is somebody's progress: the
    /// attempt's own turn once the record has shown it, the failed turn
    /// before that.
    fn anchor(&self) -> &str {
        self.attempts
            .last()
            .filter(|a| a.disposition == Disposition::Started)
            .and_then(|a| a.started.as_deref())
            .unwrap_or(&self.failed_turn)
    }
}

/// What the monitor found when it checked the agent just now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    Go,
    /// Not yet: a question or block is pending, or attendance is live.
    Hold,
    /// Auto mode was turned off.
    AutoOff,
    /// The record could not be read just now, so nothing new can be ruled
    /// out.
    Unobserved,
    /// An ordinary wait registered: the agent's turn ended the normal way.
    Deliverer,
    /// The binding moved to another session.
    SessionChanged,
    /// The agent has no pending work any more.
    ObligationCleared,
}

/// A transport's answer to one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Answer {
    /// Accepted. `started` when the transport knows the turn it began.
    Submitted { started: Option<String> },
    /// The target was not idle at the failed turn when it checked.
    Refused,
    /// The transport was not there to try; nothing was sent.
    Unavailable,
    /// It may have reached the harness. Never replayed.
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Effect {
    Dispatch { n: u32, tag: String, target: String },
    Notice(String),
}

/// What a decision needs besides the episode and the snapshot.
pub(crate) struct Ctx<'a> {
    pub(crate) now: u64,
    pub(crate) session: &'a str,
    pub(crate) tool: Tool,
    pub(crate) transport: Option<Transport>,
    /// Why `transport` is `None`, for the agent's notice.
    pub(crate) no_transport: &'static str,
    /// What the monitor found when it checked, where it checked.
    pub(crate) gate: Option<Gate>,
}

/// Why an agent with no transport is left to a person.
pub(crate) const NOT_ENABLED: &str = "automatic retry is not enabled for this harness";
pub(crate) const AUTO_OFF: &str = "auto mode is off";

/// How long a claimed attempt has for its transport to answer.
pub(crate) const ANSWER_WITHIN: u64 = 60;
/// How long a submitted attempt has to show up in the record.
pub(crate) const CONFIRM_WITHIN: u64 = 120;
/// How often a held or transportless episode is looked at again, and how
/// many absent transports it tolerates.
const RETRY_LOOK: u64 = 60;
const UNAVAILABLE_LIMIT: usize = 3;

/// The episode the agent's current state calls for.
///
/// Decided from the whole snapshot, so a batch that read a failure and a
/// person's next turn together decides once, on the person's turn. Effects
/// describe the resulting state only.
pub(crate) fn decide(
    prev: Option<Episode>,
    snap: &Snapshot,
    ctx: &Ctx,
) -> (Option<Episode>, Vec<Effect>) {
    if matches!(
        ctx.gate,
        Some(Gate::SessionChanged | Gate::ObligationCleared | Gate::Deliverer)
    ) {
        // Nothing is stuck any more. Forgotten rather than closed: if the
        // same failed turn is stuck again later, it is a new episode.
        return (None, Vec::new());
    }
    let before = prev.clone();
    let mut ep = prev.filter(|e| e.session == ctx.session);

    if let Some(e) = ep.as_mut().filter(|e| e.live()) {
        follow(e, snap, ctx);
    }
    if ep.as_ref().is_none_or(|e| !e.live())
        && let Some(latest) = snap.latest()
        && let Outcome::Failed(class) = &latest.outcome
        && !latest.retrying
        && ep.as_ref().is_none_or(|closed| !closed.owns(&latest.id))
    {
        ep = Some(opened(latest.id.clone(), class.clone(), ctx));
    }

    let mut effects = Vec::new();
    if let Some(e) = ep.as_mut().filter(|e| e.live())
        && let Some(fx) = due(e, snap, ctx)
    {
        effects.push(fx);
    }
    effects.extend(notice(before.as_ref(), ep.as_ref()).map(Effect::Notice));
    (ep, effects)
}

/// What a person is told by the move from `prev` to `next`: the notice of
/// an episode that has just escalated, and nothing for one already told.
fn notice(prev: Option<&Episode>, next: Option<&Episode>) -> Option<String> {
    let next = next.filter(|e| e.state == State::Escalated)?;
    let told = prev.is_some_and(|p| {
        p.state == State::Escalated && p.session == next.session && p.opened_by == next.opened_by
    });
    (!told).then(|| next.notice.clone()).flatten()
}

impl Episode {
    /// Whether this episode has already dealt with the turn `id`.
    fn owns(&self, id: &str) -> bool {
        self.opened_by == id
            || self.failed_turn == id
            || self
                .attempts
                .iter()
                .any(|a| a.started.as_deref() == Some(id))
    }
}

fn opened(id: String, class: String, ctx: &Ctx) -> Episode {
    let mut e = Episode {
        session: ctx.session.to_string(),
        opened_by: id.clone(),
        failed_turn: id,
        class: class.clone(),
        attempts: Vec::new(),
        next_at: None,
        state: State::Waiting,
        notice: None,
        unobserved: 0,
    };
    if ctx.transport.is_none() {
        escalate(&mut e, ctx, ctx.no_transport);
        return e;
    }
    match retries(&class).first() {
        None => escalate(&mut e, ctx, "this kind of failure needs a person"),
        Some(wait) => e.next_at = Some(ctx.now + wait),
    }
    e
}

fn escalate(e: &mut Episode, ctx: &Ctx, why: &str) {
    e.notice = Some(format!(
        "{}'s turn failed ({}) and {why}; its pending work is waiting for you",
        ctx.tool.as_str(),
        e.class
    ));
    e.state = State::Escalated;
    e.next_at = None;
}

/// Bring a live episode up to what the record now says.
fn follow(e: &mut Episode, snap: &Snapshot, ctx: &Ctx) {
    if snap
        .find(&e.failed_turn)
        .is_some_and(|t| matches!(t.outcome, Outcome::Completed | Outcome::Interrupted))
    {
        // The harness's own retry finished it, or somebody stopped it.
        close(e);
        return;
    }
    for turn in snap.after(e.anchor()) {
        if let Some(a) = e.attempts.last_mut()
            && a.made(turn)
        {
            // Clank's own turn, whenever it shows up: a late one after an
            // escalation is still the uncertain attempt, never a reset.
            a.started = Some(turn.id.clone());
            a.disposition = Disposition::Started;
            if e.state == State::Dispatched {
                e.state = State::Running;
                e.next_at = None;
            }
            continue;
        }
        match e.attempts.last() {
            Some(a) if e.state == State::Dispatched && a.disposition == Disposition::Claimed => {
                // Until the transport answers, this may be the attempt's
                // own turn under an id only the answer will name.
                return;
            }
            Some(a) if a.disposition == Disposition::Indeterminate && a.started.is_none() => {
                // May be the attempt's turn under an id nobody named. Only
                // its finishing proves the agent is going again; its
                // failing must not buy a fresh budget.
                if matches!(turn.outcome, Outcome::Completed | Outcome::Interrupted) {
                    close(e);
                    return;
                }
                continue;
            }
            _ => {}
        }
        // A start nobody can attribute to clank is a person, or the
        // harness, taking the agent back.
        close(e);
        return;
    }
    let Some(turn) = e
        .attempts
        .last()
        .filter(|a| a.disposition == Disposition::Started)
        .and_then(|a| a.started.as_deref())
        .and_then(|id| snap.find(id))
    else {
        return;
    };
    match (&turn.outcome, e.state) {
        (Outcome::Running, _) => {}
        (Outcome::Completed | Outcome::Interrupted, _) => close(e),
        (Outcome::Failed(class), State::Running) => {
            e.failed_turn = turn.id.clone();
            e.class = class.clone();
            let budget = retries(class);
            match budget.get(e.spent()) {
                _ if ctx.transport.is_none() => escalate(e, ctx, ctx.no_transport),
                Some(wait) => {
                    e.state = State::Waiting;
                    e.next_at = Some(ctx.now + wait);
                }
                None if budget.is_empty() => {
                    escalate(e, ctx, "this kind of failure needs a person")
                }
                None => escalate(e, ctx, "retrying did not recover it"),
            }
        }
        // Already decided: waiting for the next attempt, or latched for a
        // person, whom a late failure of the uncertain attempt changes
        // nothing for.
        (Outcome::Failed(_), _) => {}
    }
}

impl Attempt {
    /// Whether `turn` is the one this attempt started, by the evidence the
    /// attempt left: the id its transport named, its tag as the message id,
    /// or its tag in the wake's text. An attempt that never reached the
    /// harness made nothing.
    fn made(&self, turn: &TurnRec) -> bool {
        !matches!(
            self.disposition,
            Disposition::Unavailable | Disposition::Refused
        ) && (self.started.as_deref() == Some(turn.id.as_str())
            || self.tag == turn.id
            || turn.text.as_deref().is_some_and(|t| t.contains(&self.tag)))
    }
}

fn close(e: &mut Episode) {
    e.state = State::Closed;
    e.next_at = None;
}

/// Whatever a passed deadline calls for: a retry, or the end of a wait.
fn due(e: &mut Episode, snap: &Snapshot, ctx: &Ctx) -> Option<Effect> {
    if e.next_at.is_none_or(|at| at > ctx.now) {
        return None;
    }
    match e.state {
        State::Waiting => {
            if ctx.gate == Some(Gate::Unobserved) {
                e.unobserved += 1;
                if e.unobserved as usize >= UNAVAILABLE_LIMIT {
                    escalate(e, ctx, "its record could not be read to retry it safely");
                } else {
                    e.next_at = Some(ctx.now + RETRY_LOOK);
                }
                return None;
            }
            if ctx.gate != Some(Gate::Go) {
                e.next_at = Some(ctx.now + RETRY_LOOK);
                return None;
            }
            let Some(transport) = ctx.transport else {
                escalate(e, ctx, ctx.no_transport);
                return None;
            };
            let current = snap.latest().is_some_and(|t| {
                t.id == e.failed_turn && matches!(t.outcome, Outcome::Failed(_)) && !t.retrying
            });
            if !current {
                e.next_at = Some(ctx.now + RETRY_LOOK);
                return None;
            }
            let n = e.attempts.iter().map(|a| a.n).max().unwrap_or(0) + 1;
            let tag = format!("clank-recovery-{}-{n}", e.opened_by);
            e.attempts.push(Attempt {
                n,
                claimed_at: ctx.now,
                transport: transport.name().into(),
                target: e.failed_turn.clone(),
                tag: tag.clone(),
                started: None,
                disposition: Disposition::Claimed,
            });
            e.state = State::Dispatched;
            e.next_at = Some(ctx.now + ANSWER_WITHIN);
            Some(Effect::Dispatch {
                n,
                tag,
                target: e.failed_turn.clone(),
            })
        }
        State::Dispatched => {
            let why = match e.attempts.last_mut() {
                Some(a) if a.disposition == Disposition::Claimed => "a retry was never answered",
                _ => "a retry was never seen to start",
            };
            if let Some(a) = e.attempts.last_mut() {
                a.disposition = Disposition::Indeterminate;
            }
            escalate(e, ctx, why);
            None
        }
        State::Running | State::Escalated | State::Closed => None,
    }
}

/// A transport's answer, applied only to the attempt it was given.
pub(crate) fn answered(
    mut e: Episode,
    n: u32,
    answer: Answer,
    ctx: &Ctx,
) -> (Episode, Vec<Effect>) {
    let current = e.state == State::Dispatched
        && e.attempts
            .last()
            .is_some_and(|a| a.n == n && a.disposition == Disposition::Claimed);
    if !current {
        return (e, Vec::new());
    }
    let a = e.attempts.last_mut().expect("checked");
    match answer {
        Answer::Submitted { started } => {
            a.disposition = Disposition::Submitted;
            a.started = started;
            e.next_at = Some(a.claimed_at + CONFIRM_WITHIN);
            (e, Vec::new())
        }
        Answer::Refused => {
            a.disposition = Disposition::Refused;
            close(&mut e);
            (e, Vec::new())
        }
        Answer::Unavailable => {
            a.disposition = Disposition::Unavailable;
            if e.unavailable() >= UNAVAILABLE_LIMIT {
                escalate(&mut e, ctx, "its recovery transport never answered");
                let text = e.notice.clone().expect("escalated");
                return (e, vec![Effect::Notice(text)]);
            }
            e.state = State::Waiting;
            e.next_at = Some(ctx.now + RETRY_LOOK);
            (e, Vec::new())
        }
        Answer::Indeterminate => {
            a.disposition = Disposition::Indeterminate;
            escalate(&mut e, ctx, "a retry may or may not have reached it");
            let text = e.notice.clone().expect("escalated");
            (e, vec![Effect::Notice(text)])
        }
    }
}

/// What the monitor knows about one roster agent at a fold.
#[derive(Debug, Clone)]
pub(crate) struct Facts {
    pub(crate) author: clank_core::ids::AgentLabel,
    pub(crate) agent_dir: PathBuf,
    pub(crate) session: clank_core::agent_config::Session,
    pub(crate) auto: bool,
    /// A deliverer is registered: an ordinary wait that can take work.
    pub(crate) delivering: bool,
    /// A newly arriving waiter would be handed work.
    pub(crate) obligated: bool,
    /// `recovery.stall_after`, in seconds; `None` is `off`.
    pub(crate) stall_after: Option<u64>,
    /// A question or block is pending, or attendance is proven: the agent
    /// is not stalled, whatever its record says.
    pub(crate) attending: bool,
    /// The agent is waiting on a question it asked: its record is read for
    /// what ends it, whether or not the agent owes anything. A cancelled
    /// question has no hook and no Stop, only its result in the record.
    pub(crate) asking: bool,
}

/// What the monitor must do for an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Act {
    Dispatch {
        author: clank_core::ids::AgentLabel,
        /// The attempt, as the episode knows it: answers must name it.
        attempt: AttemptRef,
        transport: Transport,
        tag: String,
        target: String,
    },
    Notice {
        author: clank_core::ids::AgentLabel,
        /// Identifies the episode, so a notice is written once.
        key: String,
        text: String,
    },
    /// Watch, or stop watching, where a record lives.
    Watch(WatchKey),
    Unwatch(WatchKey),
}

/// A directory watched for a record's changes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct WatchKey {
    pub(crate) dir: PathBuf,
    pub(crate) recursive: bool,
}

/// What a record watch saw.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RecordEvent {
    /// Something under the watch changed: `path`, or the watched directory
    /// itself once the watch is in place.
    Changed { watch: WatchKey, path: PathBuf },
    /// The watch could not be set up, or its backend gave up.
    Failed { watch: WatchKey },
}

/// Which attempt an answer is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttemptRef {
    pub(crate) session: String,
    pub(crate) opened_by: String,
    pub(crate) n: u32,
}

/// One agent's record and episode.
struct Watched {
    tool: Tool,
    session: String,
    agent_dir: PathBuf,
    auto: bool,
    /// Whether anything is decided from the record, or it is read only for
    /// what ends a question.
    obligated: bool,
    /// Resolved once per binding, not on every fold: resolving a Codex
    /// rollout walks its session directories.
    source: Option<(clank_core::agent_config::Session, Option<Source>)>,
    watch: Option<WatchKey>,
    read: Option<Reading>,
    snap: Snapshot,
    episode: Option<Episode>,
    /// The episode could not be stored: nothing is decided again before
    /// this, so a failing disk is not retried in a tight loop, nor by the
    /// events its own failed writes cause.
    held_until: Option<u64>,
    /// A decision is owed once the hold ends, whether or not there is an
    /// episode with a deadline to carry it.
    owed: bool,
    /// Answers that arrived during a hold, or whose decision could not be
    /// stored, applied in order by the next decision that is.
    answers: Vec<(AttemptRef, Answer)>,
    /// The stall as last observed and decided, which a storage hold does
    /// not hold back.
    stall: Option<Stall>,
    /// The stall as last stored: what its notice and status are told from.
    stall_saved: Option<Stall>,
    /// The stall interval while the agent is eligible for one, `None` while
    /// it is not: what a stall decision is rebuilt from, whenever it is made.
    stall_policy: Option<u64>,
    /// The stall could not be stored: it is decided again at this time, by
    /// the deadline path, and by nothing before it.
    stall_held: Option<u64>,
}

struct Reading {
    path: PathBuf,
    tail: crate::cli::web::transcript::Tail,
    reader: RecordReader,
}

/// Every agent's recovery, as the monitor's loop drives it.
///
/// A record is armed only while its agent is obligated and has no
/// deliverer. It is read once when armed, then only when its watch reports a
/// change, and once more before a deadline decides.
pub(crate) struct Driver {
    agents: std::collections::BTreeMap<clank_core::ids::AgentLabel, Watched>,
    /// The watches the watcher holds. The driver is their one owner, and
    /// agents sharing a directory share one watch.
    installed: std::collections::BTreeSet<WatchKey>,
    /// Watches lost since the last fold, installed again only by the next
    /// one: a backend that keeps failing must not be retried in a loop.
    lost: std::collections::BTreeSet<WatchKey>,
    cwd: PathBuf,
    home: PathBuf,
}

impl Driver {
    pub(crate) fn new(cwd: PathBuf, home: PathBuf) -> Self {
        Self {
            agents: Default::default(),
            installed: Default::default(),
            lost: Default::default(),
            cwd,
            home,
        }
    }

    /// The soonest moment the loop must call [`Driver::due`].
    pub(crate) fn next_due(&self) -> Option<u64> {
        self.agents
            .values()
            .flat_map(|w| {
                let held = w.held_until.unwrap_or(0);
                let episode = if w.owed {
                    Some(held)
                } else {
                    w.episode
                        .as_ref()
                        .filter(|e| e.live())
                        .and_then(|e| e.next_at)
                        .map(|at| at.max(held))
                };
                [episode, w.stall_due()]
            })
            .flatten()
            .min()
    }

    /// Bring every agent in line with the latest fold.
    pub(crate) fn reconcile(&mut self, facts: Vec<Facts>, now: u64) -> Vec<Act> {
        let mut acts = Vec::new();
        let present: Vec<_> = facts.iter().map(|f| f.author.clone()).collect();
        let gone: Vec<_> = self
            .agents
            .keys()
            .filter(|a| !present.contains(a))
            .cloned()
            .collect();
        for author in gone {
            self.agents.remove(&author);
        }
        for f in facts {
            let session = f.session.id.as_str().to_string();
            let w = self
                .agents
                .entry(f.author.clone())
                .or_insert_with(|| Watched {
                    tool: f.session.tool,
                    session: session.clone(),
                    agent_dir: f.agent_dir.clone(),
                    auto: f.auto,
                    obligated: f.obligated,
                    source: None,
                    watch: None,
                    read: None,
                    snap: Snapshot::default(),
                    episode: load(&f.agent_dir),
                    held_until: None,
                    owed: false,
                    answers: Vec::new(),
                    stall: load_stall(&f.agent_dir),
                    stall_saved: load_stall(&f.agent_dir),
                    stall_policy: None,
                    stall_held: None,
                });
            w.auto = f.auto;
            w.obligated = f.obligated && !f.delivering;
            if w.session != session {
                acts.extend(decision(
                    &f.author,
                    w,
                    now,
                    Some(Gate::SessionChanged),
                    None,
                ));
                disarm(w);
                w.session = session;
                w.tool = f.session.tool;
                w.snap = Snapshot::default();
            }
            if !w.obligated {
                let gate = if f.delivering {
                    Gate::Deliverer
                } else {
                    Gate::ObligationCleared
                };
                acts.extend(decision(&f.author, w, now, Some(gate), None));
                w.stall_policy = None;
                acts.extend(set_stall(&f.author, w, None, now));
                if f.asking {
                    watch_record(w, &f.session, &self.cwd, &self.home);
                } else {
                    disarm(w);
                }
                continue;
            }
            let armed = watch_record(w, &f.session, &self.cwd, &self.home);
            if armed {
                acts.extend(decision(&f.author, w, now, None, None));
            }
            w.stall_policy = f.stall_after.filter(|_| f.auto && !f.attending);
            let stall = stall_wanted(w, now, armed);
            acts.extend(set_stall(&f.author, w, stall, now));
        }
        self.lost.clear();
        acts.extend(self.sync_watches());
        acts
    }

    /// Install the watches agents need and release the rest. One watch per
    /// directory however many agents need it, so a release can never leave
    /// another agent unwatched.
    fn sync_watches(&mut self) -> Vec<Act> {
        let wanted: std::collections::BTreeSet<WatchKey> = self
            .agents
            .values()
            .filter_map(|w| w.watch.clone())
            .filter(|watch| !self.lost.contains(watch))
            .collect();
        let mut acts: Vec<Act> = self
            .installed
            .difference(&wanted)
            .cloned()
            .map(Act::Unwatch)
            .collect();
        acts.extend(wanted.difference(&self.installed).cloned().map(Act::Watch));
        self.installed = wanted;
        acts
    }

    /// Something changed under a watch: whichever records it covers are
    /// found, armed or read, and decided on.
    pub(crate) fn changed(&mut self, event: RecordEvent, now: u64) -> Vec<Act> {
        let mut acts = Vec::new();
        match &event {
            RecordEvent::Failed { watch } => {
                // The watcher has let it go; the next fold installs it again.
                eprintln!(
                    "monitor: recovery lost its watch on {}",
                    watch.dir.display()
                );
                self.installed.remove(watch);
                self.lost.insert(watch.clone());
            }
            RecordEvent::Changed { watch, path } => {
                for (author, w) in self.agents.iter_mut() {
                    if w.watch.as_ref() != Some(watch) {
                        continue;
                    }
                    if let Some((session, Some(source))) = &mut w.source {
                        if let Some(found) = source.found(path, session.id.as_str()) {
                            *source = Source::At(found);
                        } else if path == &watch.dir
                            && let Source::Find { .. } = source
                            && let Some(again) =
                                record_source(&w.agent_dir, session, &self.cwd, &self.home)
                        {
                            // The watch is now in place: a rollout written
                            // before it was, and quiet since, is found once
                            // here, never again by scanning.
                            *source = again;
                        }
                    }
                    let armed = ensure(w);
                    if (armed || refresh(w)) && w.obligated {
                        acts.extend(decision(author, w, now, None, None));
                    }
                    if let Some(s) = w.stall.clone()
                        && (armed || s.state == StallState::Quiet || s.told())
                        && record_mark(w) != s.mark
                        && !w.told_latched()
                    {
                        // The record moved after a stop, or is another
                        // record: counted from here.
                        let again = Stall::waiting(&s.session, now, s.after, record_mark(w));
                        acts.extend(set_stall(author, w, Some(again), now));
                    }
                }
            }
        }
        acts.extend(self.sync_watches());
        acts
    }

    /// Fire every deadline that has passed, after reading the record and
    /// asking `gate` about the agent as it is now.
    pub(crate) fn due(
        &mut self,
        now: u64,
        gate: impl Fn(&clank_core::ids::AgentLabel, &str) -> Gate,
    ) -> Vec<Act> {
        let mut acts = Vec::new();
        for (author, w) in self.agents.iter_mut() {
            let due = w
                .episode
                .as_ref()
                .filter(|e| e.live())
                .and_then(|e| e.next_at)
                .is_some_and(|at| at <= now)
                || w.owed;
            let due = due && w.held_until.is_none_or(|at| at <= now);
            let stalled = w.stall_due().is_some_and(|at| at <= now);
            if !due && !stalled {
                continue;
            }
            let fresh = refresh(w);
            let mut g = gate(author, &w.session);
            if stalled {
                acts.extend(stall_expired(author, w, now, g));
            }
            if !due {
                continue;
            }
            if g == Gate::AutoOff {
                w.auto = false;
                g = Gate::Go;
            }
            if g == Gate::Go && !fresh {
                g = Gate::Unobserved;
            }
            acts.extend(decision(author, w, now, Some(g), None));
        }
        acts
    }

    /// A transport's answer, applied only to the attempt it was for.
    pub(crate) fn answer(
        &mut self,
        author: &clank_core::ids::AgentLabel,
        attempt: &AttemptRef,
        answer: Answer,
        now: u64,
    ) -> Vec<Act> {
        match self.agents.get_mut(author) {
            Some(w) => decision(author, w, now, None, Some((attempt, answer))),
            None => Vec::new(),
        }
    }

    /// Answers helpers left beside their agents.
    pub(crate) fn replies(&mut self, now: u64) -> Vec<Act> {
        let mut acts = Vec::new();
        for (author, w) in self.agents.iter_mut() {
            let Some(reply) = take_reply(&w.agent_dir) else {
                continue;
            };
            let attempt = AttemptRef {
                session: reply.session.clone(),
                opened_by: reply.opened_by.clone(),
                n: reply.n,
            };
            acts.extend(decision(
                author,
                w,
                now,
                None,
                Some((&attempt, reply.answer())),
            ));
        }
        acts
    }
}

/// Keep the agent's watch on where its record lives, and its reading on
/// the record that is there now. Whether a record was newly read.
/// Keep `w`'s record read and watched; whether it was (re)armed. The
/// watch reports what changes: nothing is looked at again on a fold.
fn watch_record(
    w: &mut Watched,
    session: &clank_core::agent_config::Session,
    cwd: &Path,
    home: &Path,
) -> bool {
    let rebound = w.source.as_ref().is_none_or(|(s, _)| s != session);
    if rebound {
        // A new binding, or a new record for the same session.
        let source = record_source(&w.agent_dir, session, cwd, home);
        w.source = Some((session.clone(), source));
    }
    (rebound || w.watch.is_none()) && ensure(w)
}

fn ensure(w: &mut Watched) -> bool {
    let Some((_, Some(source))) = &w.source else {
        disarm(w);
        return false;
    };
    w.watch = Some(source.watch());
    let Source::At(path) = source else {
        return false;
    };
    if w.read.as_ref().is_some_and(|r| &r.path == path) {
        return false;
    }
    let path = path.clone();
    w.read = None;
    w.snap = Snapshot::default();
    arm(w, path)
}

fn arm(w: &mut Watched, path: PathBuf) -> bool {
    let Ok((tail, reader, snap)) = bootstrap(&path, w.tool, &w.session) else {
        return false;
    };
    w.read = Some(Reading { path, tail, reader });
    w.snap = snap;
    let seen = w.snap.ids().map(str::to_string).collect();
    question_evidence(w, seen, Vec::new(), true);
    true
}

/// What a Claude agent's record says about the questions it asked: the
/// turns `seen` begin, in the record's order, are behind the newest; the
/// calls it `returned` are over; and a failed newest turn ends that turn's
/// questions (a provider error skips Stop). Attributed by the record's own
/// order and keys, never by the failure, which names no turn. On `arming`,
/// each question still pending is looked for in the record itself, once.
fn question_evidence(
    w: &Watched,
    seen: Vec<String>,
    returned: Vec<super::question_state::Key>,
    arming: bool,
) {
    use super::question_state::{Observation, load, observe, recorded_end};
    if w.tool != Tool::Claude || !super::question_state::path(&w.agent_dir).exists() {
        return;
    }
    let pending: Vec<super::question_state::Key> = load(&w.agent_dir)
        .map(|s| s.pending.into_iter().map(|q| q.key).collect())
        .unwrap_or_default();
    // Only a pending call's result is news: a hook ran before the record
    // published any call, so every other one is already recorded.
    let mut evidence: Vec<Observation> = returned
        .into_iter()
        .filter(|k| pending.contains(k))
        .map(|key| Observation::Result { key })
        .collect();
    if let Some(latest) = w.snap.latest() {
        evidence.push(Observation::Behind {
            session: w.session.clone(),
            older: seen.into_iter().filter(|id| *id != latest.id).collect(),
        });
        if matches!(latest.outcome, Outcome::Failed(_)) {
            evidence.push(Observation::TurnOver {
                session: w.session.clone(),
                prompt: latest.id.clone(),
            });
        }
    }
    if let (true, Some(read)) = (arming, &w.read) {
        evidence.extend(
            pending
                .iter()
                .filter(|k| k.session == w.session)
                .filter_map(|k| recorded_end(&read.path, k)),
        );
    }
    observe(&w.agent_dir, evidence);
}

fn disarm(w: &mut Watched) {
    w.read = None;
    w.watch = None;
}

/// Read what the record gained. Whether the snapshot is now known to be
/// current: a record that could not be read rules nothing out.
fn refresh(w: &mut Watched) -> bool {
    let Some(r) = w.read.as_mut() else {
        return false;
    };
    let Ok(polled) = r.tail.poll() else {
        return false;
    };
    if polled.reset {
        // Replaced or compacted: what was read before describes a record
        // that no longer exists.
        let path = r.path.clone();
        w.read = None;
        return arm(w, path);
    }
    let mut seen: Vec<String> = w.snap.ids().map(str::to_string).collect();
    let mut returned = Vec::new();
    for line in &polled.lines {
        if w.tool == Tool::Claude {
            returned.extend(super::question_state::results_in(&line.text, &w.session));
        }
        if let Some(fact) = r.reader.read(&line.text) {
            if let Fact::Started { id, .. } = &fact
                && !seen.contains(id)
            {
                seen.push(id.clone());
            }
            w.snap.apply(fact);
        }
    }
    question_evidence(w, seen, returned, false);
    true
}

/// Decide for one agent, persist the result, and say what must be done.
///
/// A dispatch is returned only once the claim behind it is on disk: a side
/// effect with no durable claim could be repeated after a restart.
fn decision(
    author: &clank_core::ids::AgentLabel,
    w: &mut Watched,
    now: u64,
    gate: Option<Gate>,
    answer: Option<(&AttemptRef, Answer)>,
) -> Vec<Act> {
    let transport = if w.auto { transport_for(w.tool) } else { None };
    let ctx = Ctx {
        now,
        session: &w.session,
        tool: w.tool,
        transport,
        no_transport: if w.auto { NOT_ENABLED } else { AUTO_OFF },
        gate,
    };
    w.answers
        .extend(answer.map(|(a, answer)| (a.clone(), answer)));
    if w.held_until.is_some_and(|at| at > now) {
        // Decided when the hold ends, from the state then.
        w.owed = true;
        return Vec::new();
    }
    let prev = w.episode.clone();
    let mut ep = prev.clone();
    let mut effects = Vec::new();
    for (attempt, answer) in w.answers.clone() {
        ep = match ep {
            Some(e) if e.session == attempt.session && e.opened_by == attempt.opened_by => {
                let (e, fx) = answered(e, attempt.n, answer, &ctx);
                effects.extend(fx);
                Some(e)
            }
            other => other,
        };
    }
    // An answer may name a turn the record has already shown.
    let (next, fx) = decide(ep, &w.snap, &ctx);
    effects.extend(fx);
    if next != prev
        && let Err(e) = store(&w.agent_dir, next.as_ref())
    {
        eprintln!("monitor: could not record `{author}`'s recovery, so nothing was done: {e}");
        w.held_until = Some(now + RETRY_LOOK);
        w.owed = true;
        return Vec::new();
    }
    w.held_until = None;
    w.owed = false;
    w.answers.clear();
    if next == prev {
        return Vec::new();
    }
    // Told from where the stored state was to where it is now, never from
    // a step on the way: an escalation the record has since overtaken was
    // never stored, and is never told.
    let told = notice(prev.as_ref(), next.as_ref());
    w.episode = next;
    let ep = w.episode.as_ref();
    let mut acts: Vec<Act> = told
        .zip(ep)
        .map(|(text, e)| Act::Notice {
            author: author.clone(),
            key: format!("{}:{}", e.session, e.opened_by),
            text,
        })
        .into_iter()
        .collect();
    acts.extend(effects.into_iter().filter_map(|effect| match effect {
        Effect::Notice(_) => None,
        Effect::Dispatch { n, tag, target } => {
            let e = ep.expect("a dispatch has an episode");
            transport.map(|transport| Act::Dispatch {
                author: author.clone(),
                attempt: AttemptRef {
                    session: e.session.clone(),
                    opened_by: e.opened_by.clone(),
                    n,
                },
                transport,
                tag,
                target,
            })
        }
    }));
    acts
}

/// The fallback for an eligible agent whose record shows no failure: told
/// once, after `after` seconds in which its record has not moved. Notice
/// only; it never starts or stops a turn.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Stall {
    pub(crate) session: String,
    /// When the record was last seen to move, or eligibility began.
    pub(crate) since: u64,
    pub(crate) after: u64,
    /// The record as it was at `since`; `None` when there was none to read.
    pub(crate) mark: Option<Mark>,
    pub(crate) state: StallState,
}

/// Which record, and how far it had got: any difference is progress, an
/// append or a slow tool's output as much as a replaced file of the same
/// size.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Mark {
    pub(crate) path: PathBuf,
    pub(crate) ino: u64,
    pub(crate) len: u64,
    pub(crate) modified_ns: Option<u128>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum StallState {
    Waiting,
    /// The latest turn was stopped, or failed and is failed-turn recovery's.
    /// Nothing until the record moves.
    Quiet,
    /// Latched until the agent stops being eligible.
    Told {
        notice: String,
    },
}

impl Stall {
    fn waiting(session: &str, now: u64, after: u64, mark: Option<Mark>) -> Self {
        Stall {
            session: session.to_string(),
            since: now,
            after,
            mark,
            state: StallState::Waiting,
        }
    }

    fn told(&self) -> bool {
        matches!(self.state, StallState::Told { .. })
    }

    fn same_interval(&self, other: &Stall) -> bool {
        self.session == other.session && self.since == other.since
    }
}

impl Watched {
    /// The latch: a told stall that is stored for this same interval. One
    /// not yet stored is only a candidate, which the record or eligibility
    /// may still overtake.
    fn told_latched(&self) -> bool {
        match (&self.stall, &self.stall_saved) {
            (Some(s), Some(saved)) => s.told() && saved.told() && s.same_interval(saved),
            _ => false,
        }
    }

    /// When the stall must be looked at: its deadline, or the end of a
    /// storage hold that left a decision owed.
    fn stall_due(&self) -> Option<u64> {
        if let Some(held) = self.stall_held {
            return Some(held);
        }
        self.stall
            .as_ref()
            .filter(|s| s.state == StallState::Waiting)
            .map(|s| s.since + s.after)
    }
}

/// The agent's record as it is now, the measure of its progress whatever
/// the record says: a slow tool's output counts.
fn record_mark(w: &Watched) -> Option<Mark> {
    use std::os::unix::fs::MetadataExt;
    let path = match (&w.read, &w.source) {
        (Some(r), _) => &r.path,
        (None, Some((_, Some(Source::At(path))))) => path,
        _ => return None,
    };
    let m = std::fs::metadata(path).ok()?;
    Some(Mark {
        path: path.clone(),
        ino: m.ino(),
        len: m.len(),
        modified_ns: m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos()),
    })
}

/// The stall the agent should have now, from its current eligibility: kept
/// across folds and restarts, begun afresh when it was not eligible before
/// or its record is another one.
fn stall_wanted(w: &Watched, now: u64, armed: bool) -> Option<Stall> {
    let after = w.stall_policy?;
    let mark = record_mark(w);
    Some(match w.stall.clone() {
        Some(s) if s.session == w.session && (!armed || w.told_latched() || mark == s.mark) => {
            Stall { after, ..s }
        }
        _ => Stall::waiting(&w.session, now, after, mark),
    })
}

/// A stall's deadline, or the end of a hold that left its decision owed:
/// rebuilt from the agent's current eligibility, and at a deadline, a person
/// is told unless the record moved, the latest turn was stopped or failed,
/// or the agent is not eligible just now.
fn stall_expired(
    author: &clank_core::ids::AgentLabel,
    w: &mut Watched,
    now: u64,
    gate: Gate,
) -> Vec<Act> {
    let latched = w.told_latched();
    let next = match stall_wanted(w, now, false) {
        Some(s) if s.state == StallState::Waiting && s.since + s.after <= now => {
            stall_at_deadline(author, w, s, now, gate)
        }
        // A told stall a hold kept from being stored is decided again, at
        // its deadline, from the record and the agent as they are now.
        Some(s) if s.told() && !latched => stall_at_deadline(
            author,
            w,
            Stall {
                state: StallState::Waiting,
                ..s
            },
            now,
            gate,
        ),
        other => other,
    };
    set_stall(author, w, next, now)
}

fn stall_at_deadline(
    author: &clank_core::ids::AgentLabel,
    w: &Watched,
    s: Stall,
    now: u64,
    gate: Gate,
) -> Option<Stall> {
    let mark = record_mark(w);
    match gate {
        Gate::Deliverer | Gate::SessionChanged | Gate::ObligationCleared | Gate::AutoOff => None,
        // Not eligible just now: counted again from here.
        Gate::Hold => Some(Stall::waiting(&s.session, now, s.after, mark)),
        Gate::Go | Gate::Unobserved => stall_verdict(author, w, s, now, mark),
    }
}

/// Whether the eligible agent's interval `s` has stalled, from its record
/// and policy as they are now: re-armed if the record moved, still waiting
/// if its current interval has not passed, quiet if the latest turn was
/// stopped or failed, and told only otherwise. The one rule every told
/// stall is made by, wherever it is decided or stored.
fn stall_verdict(
    author: &clank_core::ids::AgentLabel,
    w: &Watched,
    s: Stall,
    now: u64,
    mark: Option<Mark>,
) -> Option<Stall> {
    let after = w.stall_policy?;
    if mark != s.mark {
        return Some(Stall::waiting(&s.session, now, after, mark));
    }
    if s.since + after > now {
        return Some(Stall {
            after,
            state: StallState::Waiting,
            ..s
        });
    }
    let latest = w.snap.latest().map(|t| &t.outcome);
    let failing =
        matches!(latest, Some(Outcome::Failed(_))) || w.episode.as_ref().is_some_and(|e| e.live());
    let state = if failing || matches!(latest, Some(Outcome::Interrupted)) {
        StallState::Quiet
    } else {
        StallState::Told {
            notice: format!(
                "{author} has had pending work for {} and is not waiting for it",
                crate::cli::config::show_duration(after)
            ),
        }
    };
    Some(Stall { after, state, ..s })
}

/// Make `next` the agent's stall, and store it unless storage is held.
/// What is observed during a hold is kept, not dropped: the hold delays
/// only the write, and the write at its end is of the latest state.
fn set_stall(
    author: &clank_core::ids::AgentLabel,
    w: &mut Watched,
    next: Option<Stall>,
    now: u64,
) -> Vec<Act> {
    w.stall = next;
    save_stall(author, w, now)
}

/// Store the stall if it differs from what is stored, telling a person if
/// the stored stall has just become told: the notice follows the stored
/// transition, never a state that was overtaken before it could be saved.
fn save_stall(author: &clank_core::ids::AgentLabel, w: &mut Watched, now: u64) -> Vec<Act> {
    if w.stall_held.is_some_and(|at| at > now) {
        return Vec::new();
    }
    if let Some(s) = w.stall.clone()
        && s.told()
        && !w.told_latched()
    {
        // A notice not yet stored is still a candidate: whichever path
        // stores it, it is first judged by the record as it is now. An
        // unchanged mark means an unchanged record, so the turns already
        // read still stand.
        w.stall = stall_verdict(author, w, s, now, record_mark(w));
    }
    if w.stall == w.stall_saved {
        // Nothing owed: whatever hold there was is over.
        w.stall_held = None;
        return Vec::new();
    }
    if let Err(e) = store_stall(&w.agent_dir, w.stall.as_ref()) {
        eprintln!("monitor: could not record `{author}`'s stall, so nothing was done: {e}");
        w.stall_held = Some(now + RETRY_LOOK);
        return Vec::new();
    }
    w.stall_held = None;
    let previous = std::mem::replace(&mut w.stall_saved, w.stall.clone());
    let told_before = |s: &Stall| {
        previous
            .as_ref()
            .is_some_and(|p| p.told() && p.same_interval(s))
    };
    match &w.stall {
        Some(
            s @ Stall {
                state: StallState::Told { notice },
                ..
            },
        ) if !told_before(s) => vec![Act::Notice {
            author: author.clone(),
            key: format!("{}:stall:{}", s.session, s.since),
            text: notice.clone(),
        }],
        _ => Vec::new(),
    }
}

pub(crate) fn stall_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("stall.json")
}

pub(crate) fn load_stall(agent_dir: &Path) -> Option<Stall> {
    let raw = std::fs::read_to_string(stall_path(agent_dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn store_stall(agent_dir: &Path, stall: Option<&Stall>) -> std::io::Result<()> {
    let path = stall_path(agent_dir);
    match stall {
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
        Some(stall) => {
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(stall)?)?;
            std::fs::rename(&tmp, &path)
        }
    }
}

pub(crate) fn episode_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("recovery.json")
}

/// Where a recovery helper answers an attempt. The monitor reads it and
/// stays the only writer of the episode.
pub(crate) fn reply_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("recovery-reply.json")
}

/// What the agent was told when its episode escalated, while it still
/// stands: the reason its pending work is waiting for a person.
pub(crate) fn waiting_for_you(agent_dir: &Path) -> Option<String> {
    load(agent_dir)
        .filter(|ep| ep.state == State::Escalated)
        .and_then(|ep| ep.notice)
        .or_else(|| match load_stall(agent_dir)?.state {
            StallState::Told { notice } => Some(notice),
            _ => None,
        })
}

pub(crate) fn load(agent_dir: &Path) -> Option<Episode> {
    let raw = std::fs::read_to_string(episode_path(agent_dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Persist, or remove, the agent's episode.
pub(crate) fn store(agent_dir: &Path, ep: Option<&Episode>) -> std::io::Result<()> {
    let path = episode_path(agent_dir);
    match ep {
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
        Some(ep) => {
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(ep)?)?;
            std::fs::rename(&tmp, &path)
        }
    }
}

/// A helper's answer to one attempt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Reply {
    pub(crate) session: String,
    pub(crate) opened_by: String,
    pub(crate) n: u32,
    pub(crate) answer: ReplyAnswer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) started: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReplyAnswer {
    Submitted,
    Refused,
    Indeterminate,
}

impl Reply {
    pub(crate) fn answer(&self) -> Answer {
        match self.answer {
            ReplyAnswer::Submitted => Answer::Submitted {
                started: self.started.clone(),
            },
            ReplyAnswer::Refused => Answer::Refused,
            ReplyAnswer::Indeterminate => Answer::Indeterminate,
        }
    }
}

/// Take the helper's reply, so it is applied once.
pub(crate) fn take_reply(agent_dir: &Path) -> Option<Reply> {
    let path = reply_path(agent_dir);
    let raw = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    serde_json::from_str(&raw).ok()
}

pub(crate) fn write_reply(agent_dir: &Path, reply: &Reply) -> std::io::Result<()> {
    let path = reply_path(agent_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(reply)?)?;
    std::fs::rename(&tmp, &path)
}

/// Append one turn a harness reported for itself, for its session.
pub(crate) fn observe(agent_dir: &Path, line: &serde_json::Value) -> std::io::Result<()> {
    use std::io::Write as _;
    std::fs::create_dir_all(agent_dir)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(observed_path(agent_dir))?;
    writeln!(f, "{line}")
}

/// The wake a dispatched attempt submits: the agent's ordinary pending
/// work, carrying the attempt's tag so its turn is recognisable.
pub(crate) fn wake_text(tag: &str) -> String {
    format!(
        "Your previous turn failed on a provider error. Clank is retrying \
         ({tag}): check your pending work with `clank status` and carry on."
    )
}

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reports changes where the records recovery follows live.
///
/// Watches are set up on a thread of their own: on macOS one costs
/// seconds, which the monitor's loop cannot spend.
pub(crate) struct RecordWatch {
    commands: std::sync::mpsc::Sender<(WatchKey, bool)>,
}

type Backend = Box<dyn notify::Watcher + Send>;

/// A path as the backend reports it: canonical where it still exists, and
/// through its directory where it does not.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize()
        .ok()
        .or_else(|| {
            let dir = path.parent()?.canonicalize().ok()?;
            Some(dir.join(path.file_name()?))
        })
        .unwrap_or_else(|| path.to_path_buf())
}

impl RecordWatch {
    pub(crate) fn spawn(changed: tokio::sync::mpsc::UnboundedSender<RecordEvent>) -> Self {
        Self::spawn_with(changed, |handler| {
            Ok(Box::new(notify::recommended_watcher(handler)?) as Backend)
        })
    }

    pub(crate) fn spawn_with(
        changed: tokio::sync::mpsc::UnboundedSender<RecordEvent>,
        backend: impl FnOnce(
            Box<dyn FnMut(notify::Result<notify::Event>) + Send>,
        ) -> notify::Result<Backend>
        + Send
        + 'static,
    ) -> Self {
        let (commands, rx) = std::sync::mpsc::channel::<(WatchKey, bool)>();
        std::thread::spawn(move || {
            // The canonical directory of each installed watch: events name
            // canonical paths, the driver its own.
            let watches = std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::<
                WatchKey,
                PathBuf,
            >::new()));
            let (seen, to) = (watches.clone(), changed.clone());
            let handler = Box::new(move |res: notify::Result<notify::Event>| {
                let mut watches = seen.lock().unwrap_or_else(|p| p.into_inner());
                let event = match res {
                    Ok(event) => event,
                    Err(e) => {
                        // Blamed on the watches it names; one naming none
                        // may be any of them. Each is let go here, so the
                        // driver's next install is a real one.
                        let failed: Vec<_> = e.paths.iter().map(|p| canonical(p)).collect();
                        watches.retain(|watch, dir| {
                            let lost = failed.is_empty() || failed.contains(dir);
                            if lost {
                                let _ = to.send(RecordEvent::Failed {
                                    watch: watch.clone(),
                                });
                            }
                            !lost
                        });
                        return;
                    }
                };
                for path in event.paths {
                    let at = canonical(&path);
                    for (watch, dir) in watches.iter() {
                        let under = if watch.recursive {
                            at.starts_with(dir)
                        } else {
                            at == *dir || at.parent() == Some(dir.as_path())
                        };
                        if under {
                            let rel = at.strip_prefix(dir).unwrap_or(&at);
                            let _ = to.send(RecordEvent::Changed {
                                watch: watch.clone(),
                                path: watch.dir.join(rel),
                            });
                        }
                    }
                }
            });
            let mut watcher = match backend(handler) {
                Ok(w) => Some(w),
                Err(e) => {
                    eprintln!("monitor: recovery cannot watch records on this machine: {e}");
                    None
                }
            };
            for (watch, on) in rx {
                let installed = watches
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .contains_key(&watch);
                if !on {
                    if installed {
                        watches
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .remove(&watch);
                        if let Some(w) = watcher.as_mut() {
                            let _ = w.unwatch(&watch.dir);
                        }
                    }
                    continue;
                }
                let mode = if watch.recursive {
                    notify::RecursiveMode::Recursive
                } else {
                    notify::RecursiveMode::NonRecursive
                };
                let set = match watcher.as_mut() {
                    _ if installed => Ok(()),
                    Some(w) => {
                        // A watch the backend gave up on may still be
                        // registered with it.
                        let _ = w.unwatch(&watch.dir);
                        w.watch(&watch.dir, mode).map_err(|e| e.to_string())
                    }
                    None => Err("no watch backend".into()),
                };
                let _ = changed.send(match set {
                    // Whatever changed while the watch was being set up is
                    // read now.
                    Ok(()) => {
                        watches
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .insert(watch.clone(), canonical(&watch.dir));
                        RecordEvent::Changed {
                            path: watch.dir.clone(),
                            watch,
                        }
                    }
                    Err(e) => {
                        eprintln!("monitor: cannot watch {}: {e}", watch.dir.display());
                        RecordEvent::Failed { watch }
                    }
                });
            }
        });
        Self { commands }
    }

    pub(crate) fn watch(&self, watch: WatchKey) {
        let _ = self.commands.send((watch, true));
    }

    pub(crate) fn unwatch(&self, watch: WatchKey) {
        let _ = self.commands.send((watch, false));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    thread_local! {
        static ENABLED: std::cell::RefCell<Vec<(Tool, Option<Transport>)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// The capability this test thread pretends `tool` has passed.
    pub(crate) fn enable(tool: Tool, transport: Option<Transport>) {
        ENABLED.with(|e| e.borrow_mut().push((tool, transport)));
    }

    pub(super) fn enabled(tool: Tool) -> Option<Option<Transport>> {
        ENABLED.with(|e| {
            e.borrow()
                .iter()
                .rev()
                .find(|(t, _)| *t == tool)
                .map(|(_, x)| *x)
        })
    }

    fn ctx(now: u64, transport: Option<Transport>, gate: Option<Gate>) -> Ctx<'static> {
        Ctx {
            now,
            session: "s1",
            tool: Tool::OpenCode,
            transport,
            no_transport: NOT_ENABLED,
            gate,
        }
    }

    fn started(id: &str) -> Fact {
        Fact::Started {
            id: id.into(),
            text: None,
        }
    }

    fn ended(id: &str, outcome: Outcome) -> Fact {
        Fact::Ended {
            id: Some(id.into()),
            outcome,
        }
    }

    fn failed(id: &str, class: &str) -> Fact {
        ended(id, Outcome::Failed(class.into()))
    }

    fn snap(facts: impl IntoIterator<Item = Fact>) -> Snapshot {
        let mut s = Snapshot::default();
        for f in facts {
            s.apply(f);
        }
        s
    }

    const HELPER: Option<Transport> = Some(Transport::Helper);
    const GO: Option<Gate> = Some(Gate::Go);

    fn submitted(e: Episode, n: u32, started: Option<&str>, now: u64) -> Episode {
        let (e, fx) = answered(
            e,
            n,
            Answer::Submitted {
                started: started.map(str::to_string),
            },
            &ctx(now, HELPER, None),
        );
        assert!(fx.is_empty());
        e
    }

    #[test]
    fn classes_earn_retries_or_a_person() {
        assert_eq!(retries("server_overloaded"), &[60, 300, 900]);
        assert_eq!(retries("overloaded"), &[60, 300, 900]);
        assert_eq!(retries("max_output_tokens"), &[60]);
        assert_eq!(retries("other"), &[300]);
        assert_eq!(retries("something new"), &[300]);
        for fatal in [
            "authentication_failed",
            "billing_error",
            "usage_limit_exceeded",
        ] {
            assert!(retries(fatal).is_empty(), "{fatal}");
        }
    }

    /// Fail, retry, the retry fails: the second attempt targets the turn
    /// that failed last, not the one that opened the episode.
    #[test]
    fn a_failed_retry_is_the_next_attempts_target() {
        let mut s = snap([started("t1"), failed("t1", "server_error")]);
        let (ep, fx) = decide(None, &s, &ctx(0, HELPER, None));
        assert!(fx.is_empty());
        let ep = ep.unwrap();
        assert_eq!((ep.state, ep.next_at), (State::Waiting, Some(60)));

        let (ep, fx) = decide(Some(ep), &s, &ctx(60, HELPER, GO));
        let tag = "clank-recovery-t1-1".to_string();
        assert_eq!(
            fx,
            [Effect::Dispatch {
                n: 1,
                tag: tag.clone(),
                target: "t1".into()
            }]
        );
        let ep = submitted(ep.unwrap(), 1, Some("a1"), 61);
        s.apply(started("a1"));
        let (ep, _) = decide(Some(ep), &s, &ctx(62, HELPER, None));
        assert_eq!(ep.as_ref().unwrap().state, State::Running);

        s.apply(failed("a1", "server_error"));
        let (ep, fx) = decide(ep, &s, &ctx(70, HELPER, None));
        assert!(fx.is_empty());
        let ep = ep.unwrap();
        assert_eq!(ep.state, State::Waiting);
        assert_eq!(
            ep.next_at,
            Some(370),
            "the second wait, not the first again"
        );
        assert_eq!(ep.opened_by, "t1", "still the same episode");

        let (_, fx) = decide(Some(ep), &s, &ctx(370, HELPER, GO));
        assert_eq!(
            fx,
            [Effect::Dispatch {
                n: 2,
                tag: "clank-recovery-t1-2".into(),
                target: "a1".into()
            }]
        );
    }

    /// A retry that fails with a class no retry can fix stops there, even
    /// with budget left under the class that opened the episode.
    #[test]
    fn a_retrys_own_class_decides_what_follows_it() {
        let mut s = snap([started("t1"), failed("t1", "server_error")]);
        let ep = decide(None, &s, &ctx(0, HELPER, None)).0;
        let ep = decide(ep, &s, &ctx(60, HELPER, GO)).0.unwrap();
        let ep = submitted(ep, 1, Some("a1"), 61);
        s.apply(started("a1"));
        s.apply(failed("a1", "authentication_failed"));
        let (ep, fx) = decide(Some(ep), &s, &ctx(62, HELPER, None));
        let ep = ep.unwrap();
        assert_eq!(ep.state, State::Escalated);
        assert_eq!(ep.class, "authentication_failed");
        assert!(matches!(fx.as_slice(), [Effect::Notice(t)] if t.contains("needs a person")));
    }

    /// A batch that holds a failure and a person's next turn decides once,
    /// on the person's turn: nothing opens and nobody is told.
    #[test]
    fn a_batch_decides_on_where_it_ends() {
        let s = snap([started("t1"), failed("t1", "billing_error"), started("t2")]);
        assert_eq!(decide(None, &s, &ctx(0, None, None)), (None, Vec::new()));
    }

    /// A deadline that finds a newer turn in the record dispatches nothing.
    #[test]
    fn a_due_retry_rereads_before_it_acts() {
        let mut s = snap([started("7"), failed("7", "unknown")]);
        let ep = decide(None, &s, &ctx(0, HELPER, None)).0;
        s.apply(started("8"));
        let (ep, fx) = decide(ep, &s, &ctx(300, HELPER, GO));
        assert!(fx.is_empty());
        assert_eq!(ep.unwrap().state, State::Closed);
    }

    /// Exhausted, a duplicate report, then a person's turn that fails too.
    #[test]
    fn escalation_is_latched_until_a_new_turn_fails() {
        let mut s = snap([started("t1"), failed("t1", "max_output_tokens")]);
        let ep = decide(None, &s, &ctx(0, HELPER, None)).0;
        let ep = decide(ep, &s, &ctx(60, HELPER, GO)).0.unwrap();
        let ep = submitted(ep, 1, Some("a1"), 61);
        s.apply(started("a1"));
        s.apply(failed("a1", "max_output_tokens"));
        let (ep, fx) = decide(Some(ep), &s, &ctx(62, HELPER, None));
        assert!(matches!(fx.as_slice(), [Effect::Notice(t)] if t.contains("did not recover")));
        s.apply(failed("a1", "max_output_tokens"));
        let (ep, fx) = decide(ep, &s, &ctx(63, HELPER, GO));
        assert!(fx.is_empty(), "told once: {fx:?}");
        assert_eq!(ep.as_ref().unwrap().state, State::Escalated);

        s.apply(started("p1"));
        let (ep, fx) = decide(ep, &s, &ctx(80, None, None));
        assert!(fx.is_empty());
        assert_eq!(
            ep.as_ref().unwrap().state,
            State::Closed,
            "a person's turn is the reset"
        );
        s.apply(failed("p1", "server_error"));
        let (ep, fx) = decide(ep, &s, &ctx(90, None, None));
        assert_eq!(ep.unwrap().opened_by, "p1");
        assert!(
            matches!(fx.as_slice(), [Effect::Notice(_)]),
            "a new failure is told"
        );
    }

    /// A person's turn that fails in the same batch that ended the last
    /// escalation is a new failure, and is told.
    #[test]
    fn a_new_failure_read_with_the_reset_is_told() {
        let mut s = snap([started("t1"), failed("t1", "billing_error")]);
        let (ep, fx) = decide(None, &s, &ctx(0, None, None));
        assert_eq!(fx.len(), 1);
        s.apply(started("p1"));
        s.apply(failed("p1", "billing_error"));
        let (ep, fx) = decide(ep, &s, &ctx(10, None, None));
        assert_eq!(ep.unwrap().opened_by, "p1");
        assert!(matches!(fx.as_slice(), [Effect::Notice(_)]), "{fx:?}");
    }

    #[test]
    fn an_interruption_is_not_a_failure() {
        let s = snap([started("t1"), ended("t1", Outcome::Interrupted)]);
        assert_eq!(decide(None, &s, &ctx(2400, HELPER, GO)), (None, Vec::new()));
    }

    #[test]
    fn without_a_transport_a_failure_is_told_once_and_left_to_a_person() {
        let s = snap([started("t1"), failed("t1", "server_overloaded")]);
        let (ep, fx) = decide(None, &s, &ctx(0, None, None));
        assert!(matches!(fx.as_slice(), [Effect::Notice(t)] if t.contains("not enabled")));
        let (ep, fx) = decide(ep, &s, &ctx(5, None, GO));
        assert!(fx.is_empty());
        assert_eq!(ep.unwrap().state, State::Escalated);
    }

    #[test]
    fn a_fatal_class_escalates_without_an_attempt() {
        let s = snap([started("t1"), failed("t1", "authentication_failed")]);
        let (ep, fx) = decide(None, &s, &ctx(0, HELPER, None));
        assert!(matches!(fx.as_slice(), [Effect::Notice(_)]));
        assert!(ep.unwrap().attempts.is_empty());
    }

    /// A claim with no answer by its deadline — the monitor died, or the
    /// transport hung — may have reached the harness and is never replayed.
    #[test]
    fn an_unanswered_claim_escalates_at_its_deadline() {
        let s = snap([started("t1"), failed("t1", "server_error")]);
        let ep = decide(None, &s, &ctx(0, HELPER, None)).0;
        let ep = decide(ep, &s, &ctx(60, HELPER, GO)).0;
        assert_eq!(ep.as_ref().unwrap().next_at, Some(60 + ANSWER_WITHIN));
        let (ep, fx) = decide(ep, &s, &ctx(60 + ANSWER_WITHIN - 1, HELPER, GO));
        assert!(fx.is_empty());
        let (ep, fx) = decide(ep, &s, &ctx(60 + ANSWER_WITHIN, HELPER, GO));
        assert!(matches!(fx.as_slice(), [Effect::Notice(_)]), "{fx:?}");
        let ep = ep.unwrap();
        assert_eq!(ep.state, State::Escalated);
        assert_eq!(ep.attempts[0].disposition, Disposition::Indeterminate);
    }

    #[test]
    fn a_submitted_attempt_is_confirmed_or_escalates_within_its_bound() {
        let s = snap([started("t1"), failed("t1", "server_error")]);
        let ep = decide(None, &s, &ctx(0, HELPER, None)).0;
        let ep = decide(ep, &s, &ctx(60, HELPER, GO)).0.unwrap();
        let ep = submitted(ep, 1, None, 61);
        assert_eq!(ep.next_at, Some(60 + CONFIRM_WITHIN));
        let (_, fx) = decide(Some(ep), &s, &ctx(60 + CONFIRM_WITHIN, HELPER, GO));
        assert!(
            matches!(fx.as_slice(), [Effect::Notice(t)] if t.contains("never seen")),
            "{fx:?}"
        );
    }

    /// Until the transport answers, a new turn may be the attempt's own
    /// under an id only the answer names; it is not mistaken for a person.
    #[test]
    fn a_turn_during_an_unanswered_claim_waits_for_the_answer() {
        let mut s = snap([started("t1"), failed("t1", "server_error")]);
        let ep = decide(None, &s, &ctx(0, HELPER, None)).0;
        let ep = decide(ep, &s, &ctx(60, HELPER, GO)).0;
        s.apply(started("a1"));
        let (ep, _) = decide(ep, &s, &ctx(61, HELPER, None));
        assert_eq!(ep.as_ref().unwrap().state, State::Dispatched);
        let ep = submitted(ep.unwrap(), 1, Some("a1"), 62);
        let (ep, _) = decide(Some(ep), &s, &ctx(62, HELPER, None));
        assert_eq!(ep.unwrap().state, State::Running);
    }

    #[test]
    fn a_turn_carrying_the_attempts_tag_is_the_attempt() {
        let mut s = snap([started("t1"), failed("t1", "server_error")]);
        let ep = decide(None, &s, &ctx(0, HELPER, None)).0;
        let ep = decide(ep, &s, &ctx(60, HELPER, GO)).0.unwrap();
        let ep = submitted(ep, 1, None, 61);
        s.apply(Fact::Started {
            id: "p9".into(),
            text: Some(wake_text("clank-recovery-t1-1")),
        });
        let (ep, _) = decide(Some(ep), &s, &ctx(62, HELPER, None));
        assert_eq!(ep.unwrap().state, State::Running);
    }

    #[test]
    fn a_native_retry_holds_the_episode_and_its_success_closes_it() {
        let mut s = snap([started("t1"), failed("t1", "server_error")]);
        let ep = decide(None, &s, &ctx(0, HELPER, None)).0;
        s.apply(Fact::NativeRetry);
        let (ep, fx) = decide(ep, &s, &ctx(60, HELPER, GO));
        assert!(fx.is_empty(), "held while the harness retries");
        assert_eq!(ep.as_ref().unwrap().next_at, Some(60 + RETRY_LOOK));
        s.apply(ended("t1", Outcome::Completed));
        let (ep, fx) = decide(ep, &s, &ctx(61, HELPER, None));
        assert!(fx.is_empty());
        assert_eq!(ep.unwrap().state, State::Closed);
    }

    #[test]
    fn a_held_gate_waits_and_an_end_of_need_forgets_the_episode() {
        let s = snap([started("t1"), failed("t1", "server_error")]);
        let ep = decide(None, &s, &ctx(0, HELPER, None)).0;
        let (ep, fx) = decide(ep, &s, &ctx(60, HELPER, Some(Gate::Hold)));
        assert!(fx.is_empty());
        assert_eq!(ep.as_ref().unwrap().next_at, Some(60 + RETRY_LOOK));
        for end in [
            Gate::Deliverer,
            Gate::SessionChanged,
            Gate::ObligationCleared,
        ] {
            assert_eq!(
                decide(ep.clone(), &s, &ctx(120, HELPER, Some(end))),
                (None, Vec::new()),
                "{end:?}"
            );
        }
    }

    #[test]
    fn a_refused_attempt_closes_and_its_turn_is_never_opened_again() {
        let s = snap([started("t1"), failed("t1", "server_error")]);
        let ep = decide(None, &s, &ctx(0, HELPER, None)).0;
        let ep = decide(ep, &s, &ctx(60, HELPER, GO)).0.unwrap();
        let (ep, fx) = answered(ep, 1, Answer::Refused, &ctx(61, HELPER, None));
        assert!(fx.is_empty());
        assert_eq!(ep.state, State::Closed);
        let (ep, fx) = decide(Some(ep), &s, &ctx(5000, HELPER, GO));
        assert!(fx.is_empty());
        assert_eq!(ep.unwrap().state, State::Closed);
    }

    #[test]
    fn an_indeterminate_answer_escalates_and_is_never_replayed() {
        let s = snap([started("t1"), failed("t1", "server_error")]);
        let ep = decide(None, &s, &ctx(0, HELPER, None)).0;
        let ep = decide(ep, &s, &ctx(60, HELPER, GO)).0.unwrap();
        let (ep, fx) = answered(ep, 1, Answer::Indeterminate, &ctx(61, HELPER, None));
        assert!(matches!(fx.as_slice(), [Effect::Notice(_)]));
        let (_, fx) = decide(Some(ep), &s, &ctx(5000, HELPER, GO));
        assert!(fx.is_empty());
    }

    /// After an attempt's outcome became uncertain and escalated, its turn
    /// showing up late is still that attempt: its failure keeps the latch
    /// and the spent budget, and only its finishing closes the episode.
    #[test]
    fn a_late_uncertain_attempt_keeps_its_escalation_until_it_finishes() {
        let tagged = |id: &str| Fact::Started {
            id: id.into(),
            text: Some(wake_text("clank-recovery-t1-1")),
        };
        type Uncertain = fn(Episode) -> Episode;
        let answered_indeterminate: Uncertain =
            |ep| answered(ep, 1, Answer::Indeterminate, &ctx(61, HELPER, None)).0;
        let named_unconfirmed: Uncertain = |ep| {
            let ep = submitted(ep, 1, Some("clank-recovery-t1-1"), 61);
            let s = snap([started("t1"), failed("t1", "server_error")]);
            decide(Some(ep), &s, &ctx(60 + CONFIRM_WITHIN, HELPER, GO))
                .0
                .unwrap()
        };
        let confirm_timed_out: Uncertain = |ep| {
            let ep = submitted(ep, 1, None, 61);
            let s = snap([started("t1"), failed("t1", "server_error")]);
            decide(Some(ep), &s, &ctx(60 + CONFIRM_WITHIN, HELPER, GO))
                .0
                .unwrap()
        };
        for (how, uncertain) in [
            ("indeterminate", answered_indeterminate),
            ("unconfirmed", confirm_timed_out),
            ("named, unconfirmed", named_unconfirmed),
        ] {
            for (late, id) in [("tag id", "clank-recovery-t1-1"), ("wake text", "p7")] {
                let mut s = snap([started("t1"), failed("t1", "server_error")]);
                let ep = decide(None, &s, &ctx(0, HELPER, None)).0;
                let ep = decide(ep, &s, &ctx(60, HELPER, GO)).0.unwrap();
                let ep = uncertain(ep);
                assert_eq!(ep.state, State::Escalated, "{how}");
                s.apply(if late == "tag id" {
                    started(id)
                } else {
                    tagged(id)
                });
                s.apply(failed(id, "server_error"));
                let (after, fx) = decide(Some(ep.clone()), &s, &ctx(500, HELPER, GO));
                let after = after.unwrap();
                assert_eq!(
                    (after.state, after.opened_by.as_str()),
                    (State::Escalated, "t1"),
                    "{how} {late}"
                );
                assert_eq!(after.attempts.len(), 1, "{how} {late}: no fresh budget");
                assert!(fx.is_empty(), "{how} {late}: {fx:?}");

                let mut s = snap([started("t1"), failed("t1", "server_error")]);
                s.apply(if late == "tag id" {
                    started(id)
                } else {
                    tagged(id)
                });
                s.apply(ended(id, Outcome::Completed));
                let (after, _) = decide(Some(ep), &s, &ctx(500, HELPER, GO));
                assert_eq!(
                    after.unwrap().state,
                    State::Closed,
                    "{how} {late}: recovered"
                );
            }
        }
    }

    /// An uncertain attempt whose turn nobody named: a newer turn that fails
    /// may be it, so the latch holds; one that finishes ends the episode.
    #[test]
    fn an_unnamed_uncertain_attempt_is_not_mistaken_for_a_reset() {
        let mut s = snap([started("t1"), failed("t1", "server_error")]);
        let ep = decide(None, &s, &ctx(0, HELPER, None)).0;
        let ep = decide(ep, &s, &ctx(60, HELPER, GO)).0.unwrap();
        let ep = answered(ep, 1, Answer::Indeterminate, &ctx(61, HELPER, None)).0;
        s.apply(started("x9"));
        s.apply(failed("x9", "server_error"));
        let (after, fx) = decide(Some(ep.clone()), &s, &ctx(70, HELPER, GO));
        assert_eq!(after.unwrap().opened_by, "t1");
        assert!(fx.is_empty(), "{fx:?}");
        let mut s = snap([started("t1"), failed("t1", "server_error"), started("x9")]);
        s.apply(ended("x9", Outcome::Completed));
        assert_eq!(
            decide(Some(ep), &s, &ctx(70, HELPER, GO)).0.unwrap().state,
            State::Closed
        );
    }

    #[test]
    fn an_answer_for_another_attempt_changes_nothing() {
        let s = snap([started("t1"), failed("t1", "server_error")]);
        let ep = decide(None, &s, &ctx(0, HELPER, None)).0;
        let ep = decide(ep, &s, &ctx(60, HELPER, GO)).0.unwrap();
        let (after, fx) = answered(ep.clone(), 2, Answer::Indeterminate, &ctx(61, HELPER, None));
        assert_eq!((after, fx), (ep, Vec::new()));
    }

    #[test]
    fn an_absent_transport_is_not_an_attempt_but_cannot_wait_forever() {
        let s = snap([started("t1"), failed("t1", "server_error")]);
        let mut ep = decide(None, &s, &ctx(0, HELPER, None)).0.unwrap();
        let mut now = 60;
        let mut notices = 0;
        for _ in 0..UNAVAILABLE_LIMIT {
            ep = decide(Some(ep), &s, &ctx(now, HELPER, GO)).0.unwrap();
            let n = ep.attempts.last().unwrap().n;
            let (next, fx) = answered(ep, n, Answer::Unavailable, &ctx(now, HELPER, None));
            notices += fx.iter().filter(|f| matches!(f, Effect::Notice(_))).count();
            ep = next;
            now += RETRY_LOOK;
        }
        assert_eq!(
            ep.spent(),
            0,
            "no budget was spent on a transport that was not there"
        );
        assert_eq!(ep.state, State::Escalated);
        assert_eq!(notices, 1);
    }

    #[test]
    fn records_are_read_for_turns_and_never_for_quoted_text() {
        let read = |tool, lines: &[&str]| {
            let mut r = RecordReader::new(tool, "s1");
            lines.iter().filter_map(|l| r.read(l)).collect::<Vec<_>>()
        };
        assert_eq!(
            read(
                Tool::Codex,
                &[
                    r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"a"}}"#,
                    r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"a","last_agent_message":null,"error":{"message":"Selected model is at capacity. Please try a different model.","codex_error_info":"server_overloaded"}}}"#,
                    r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"b","error":null}}"#,
                    r#"{"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"c","reason":"interrupted"}}"#,
                    r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"{\"error\":{\"message\":\"Selected model is at capacity.\",\"codex_error_info\":\"server_overloaded\"}}"}]}}"#,
                ]
            ),
            [
                started("a"),
                failed("a", "server_overloaded"),
                ended("b", Outcome::Completed),
                ended("c", Outcome::Interrupted),
            ],
            "quoted in a message, it is something somebody said"
        );
        assert_eq!(
            read(
                Tool::Claude,
                &[
                    r#"{"type":"user","promptId":"p1","message":{"content":"do it"}}"#,
                    r#"{"type":"user","promptId":"p1","message":{"content":[{"type":"tool_result","content":"x"}]}}"#,
                    r#"{"type":"assistant","isApiErrorMessage":true,"error":"rate_limit","apiErrorStatus":429}"#,
                ]
            ),
            [
                Fact::Started {
                    id: "p1".into(),
                    text: Some("do it".into())
                },
                Fact::Ended {
                    id: None,
                    outcome: Outcome::Failed("rate_limit".into())
                },
            ],
            "a tool result belongs to the turn already started"
        );
        assert_eq!(
            read(
                Tool::Grok,
                &[
                    r#"{"type":"turn_started","turn_number":3}"#,
                    r#"{"type":"turn_ended","outcome":"error"}"#,
                    r#"{"type":"turn_ended","outcome":"cancelled","cancellation_category":"mid_turn_abort"}"#,
                ]
            ),
            [
                started("3"),
                Fact::Ended {
                    id: None,
                    outcome: Outcome::Failed("unknown".into())
                },
                Fact::Ended {
                    id: None,
                    outcome: Outcome::Interrupted
                },
            ]
        );
        assert_eq!(
            read(
                Tool::OpenCode,
                &[
                    r#"{"session":"s1","turn":"started","id":"m1"}"#,
                    r#"{"session":"s0","turn":"failed","id":"x","class":"billing_error"}"#,
                    r#"{"session":"s1","turn":"retry"}"#,
                    r#"{"session":"s1","turn":"failed","id":"m1","class":"server_error"}"#,
                    r#"{"turn":"failed","id":"m1","class":"server_error"}"#,
                ]
            ),
            [
                started("m1"),
                Fact::NativeRetry,
                failed("m1", "server_error")
            ],
            "another session's report, or one naming none, is not this session's turn"
        );
    }

    /// A bound agent whose record is the file at `record`.
    fn facts(
        tool: &str,
        dir: &Path,
        record: &Path,
        session: &str,
        obligated: bool,
        delivering: bool,
    ) -> Facts {
        let session: clank_core::agent_config::Session =
            serde_json::from_value(serde_json::json!({
                "id": session,
                "tool": tool,
                "updated_at": "2026-10-05T00:00:00Z",
                "transcript": record,
            }))
            .unwrap();
        Facts {
            author: label(),
            agent_dir: dir.to_path_buf(),
            session,
            auto: true,
            delivering,
            obligated,
            stall_after: None,
            attending: false,
            asking: false,
        }
    }

    fn codex_facts(
        dir: &Path,
        rollout: &Path,
        session: &str,
        obligated: bool,
        delivering: bool,
    ) -> Facts {
        facts("codex", dir, rollout, session, obligated, delivering)
    }

    fn label() -> clank_core::ids::AgentLabel {
        clank_core::ids::AgentLabel::parse("agent").unwrap()
    }

    fn append(path: &Path, lines: &[&str]) {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    const STARTED: &str = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"T"}}"#;
    const FAILED: &str = r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"T","error":{"message":"Selected model is at capacity.","codex_error_info":"server_overloaded"}}}"#;
    const DONE: &str =
        r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"T","error":null}}"#;

    fn line(template: &str, turn: &str) -> String {
        template.replace("\"T\"", &format!("\"{turn}\""))
    }

    fn watch_of(record: &Path) -> WatchKey {
        WatchKey {
            dir: record.parent().unwrap().to_path_buf(),
            recursive: false,
        }
    }

    /// What the record's watch reports when `record` changes.
    fn touched(d: &mut Driver, record: &Path, now: u64) -> Vec<Act> {
        d.changed(
            RecordEvent::Changed {
                watch: watch_of(record),
                path: record.to_path_buf(),
            },
            now,
        )
    }

    fn driver() -> Driver {
        Driver::new(PathBuf::from("/"), PathBuf::from("/nonexistent-home"))
    }

    fn notices(acts: &[Act]) -> Vec<&str> {
        acts.iter()
            .filter_map(|a| match a {
                Act::Notice { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn dispatches(acts: &[Act]) -> Vec<(&AttemptRef, &str)> {
        acts.iter()
            .filter_map(|a| match a {
                Act::Dispatch {
                    attempt, target, ..
                } => Some((attempt, target.as_str())),
                _ => None,
            })
            .collect()
    }

    /// The incident: a failed Codex turn in the record, an obligated agent
    /// with nothing waiting, and no transport enabled. One notice, latched,
    /// until a person's turn appears in the same record.
    #[test]
    fn a_recorded_failure_with_no_transport_is_told_once_and_a_person_ends_it() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let mut d = driver();
        let f = || {
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                true,
                false,
            )]
        };

        let acts = d.reconcile(f(), 100);
        assert!(acts.contains(&Act::Watch(watch_of(&rollout))), "{acts:?}");
        let told = notices(&acts);
        assert!(
            matches!(told.as_slice(), [t] if t.contains("server_overloaded") && t.contains("not enabled")),
            "{acts:?}"
        );
        assert_eq!(load(dir.path()).unwrap().state, State::Escalated);
        assert!(
            d.reconcile(f(), 200).is_empty(),
            "told once, and nothing re-read"
        );

        append(&rollout, &[&line(STARTED, "t2")]);
        assert!(touched(&mut d, &rollout, 300).is_empty());
        assert_eq!(
            load(dir.path()).unwrap().state,
            State::Closed,
            "a person's turn ends it"
        );
        assert_eq!(waiting_for_you(dir.path()), None);
    }

    /// With a transport enabled: the deadline dispatches against the failed
    /// turn, the attempt's turn is attributed by the id the transport
    /// returned, and its completion closes the episode.
    #[test]
    fn an_enabled_transport_retries_the_failed_turn_and_its_success_closes_it() {
        enable(Tool::Codex, Some(Transport::CodexStart));
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let mut d = driver();
        let acts = d.reconcile(
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                true,
                false,
            )],
            0,
        );
        assert_eq!(acts, [Act::Watch(watch_of(&rollout))]);
        assert_eq!(d.next_due(), Some(60));
        assert!(d.due(59, |_, _| Gate::Go).is_empty());
        let acts = d.due(60, |_, _| Gate::Go);
        let sent = dispatches(&acts);
        let [(attempt, "t1")] = sent.as_slice() else {
            panic!("{acts:?}")
        };
        let attempt = (*attempt).clone();
        assert_eq!(
            load(dir.path()).unwrap().attempts[0].disposition,
            Disposition::Claimed,
            "claimed on disk before the transport ran"
        );
        d.answer(
            &label(),
            &attempt,
            Answer::Submitted {
                started: Some("t2".into()),
            },
            61,
        );
        append(&rollout, &[&line(STARTED, "t2")]);
        touched(&mut d, &rollout, 62);
        assert_eq!(load(dir.path()).unwrap().state, State::Running);
        append(&rollout, &[&line(DONE, "t2")]);
        touched(&mut d, &rollout, 90);
        assert_eq!(load(dir.path()).unwrap().state, State::Closed, "recovered");
        assert_eq!(d.next_due(), None);
    }

    /// A due attempt reads what the record gained since its last event
    /// before deciding: a person's turn appended without a watch event
    /// still stops the dispatch.
    #[test]
    fn a_due_attempt_reads_the_record_first() {
        enable(Tool::Codex, Some(Transport::CodexStart));
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let mut d = driver();
        d.reconcile(
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                true,
                false,
            )],
            0,
        );
        append(&rollout, &[&line(STARTED, "t2")]);
        assert!(d.due(60, |_, _| Gate::Go).is_empty());
        assert_eq!(load(dir.path()).unwrap().state, State::Closed);
    }

    #[test]
    fn a_due_attempt_asks_the_gate_about_the_agent_now() {
        enable(Tool::Codex, Some(Transport::CodexStart));
        for (gate, after) in [
            (Gate::Hold, Some(State::Waiting)),
            (Gate::Deliverer, None),
            (Gate::SessionChanged, None),
            (Gate::AutoOff, Some(State::Escalated)),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let rollout = dir.path().join("rollout.jsonl");
            append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
            let mut d = driver();
            d.reconcile(
                vec![codex_facts(
                    dir.path(),
                    &rollout,
                    "session-one",
                    true,
                    false,
                )],
                0,
            );
            let acts = d.due(60, |author, session| {
                assert_eq!((author, session), (&label(), "session-one"));
                gate
            });
            assert!(dispatches(&acts).is_empty(), "{gate:?}");
            assert_eq!(load(dir.path()).map(|e| e.state), after, "{gate:?}");
            if gate == Gate::AutoOff {
                assert!(notices(&acts)[0].contains(AUTO_OFF));
            }
        }
    }

    #[test]
    fn a_deliverer_obligation_or_session_change_forgets_the_episode_and_its_watch() {
        enable(Tool::Codex, Some(Transport::CodexStart));
        for end in ["deliverer", "obligation", "session"] {
            let dir = tempfile::tempdir().unwrap();
            let rollout = dir.path().join("rollout.jsonl");
            append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
            let mut d = driver();
            d.reconcile(
                vec![codex_facts(
                    dir.path(),
                    &rollout,
                    "session-one",
                    true,
                    false,
                )],
                0,
            );
            let f = match end {
                "deliverer" => codex_facts(dir.path(), &rollout, "session-one", true, true),
                "obligation" => codex_facts(dir.path(), &rollout, "session-one", false, false),
                _ => codex_facts(dir.path(), &rollout, "session-two", false, false),
            };
            let acts = d.reconcile(vec![f], 30);
            assert_eq!(acts, [Act::Unwatch(watch_of(&rollout))], "{end}");
            assert_eq!(load(dir.path()), None, "{end}");
            assert_eq!(d.next_due(), None, "{end}");
        }
    }

    #[test]
    fn a_monitor_restarted_mid_attempt_escalates_rather_than_replaying() {
        enable(Tool::Codex, Some(Transport::CodexStart));
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let f = || {
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                true,
                false,
            )]
        };
        {
            let mut d = driver();
            d.reconcile(f(), 0);
            assert_eq!(dispatches(&d.due(60, |_, _| Gate::Go)).len(), 1);
        }
        let mut d = driver();
        let acts = d.reconcile(f(), 500);
        assert_eq!(notices(&acts).len(), 1, "{acts:?}");
        let ep = load(dir.path()).unwrap();
        assert_eq!(ep.state, State::Escalated);
        assert_eq!(ep.attempts[0].disposition, Disposition::Indeterminate);
        assert!(
            d.due(10_000, |_, _| Gate::Go).is_empty(),
            "never dispatched again"
        );
    }

    /// What happened while no monitor ran is read in order on the next
    /// arm: a submitted attempt that started and failed is a spent attempt
    /// and the next one is due, not a lost transition.
    #[test]
    fn a_restarted_monitor_follows_what_the_record_gained_meanwhile() {
        enable(Tool::Codex, Some(Transport::CodexStart));
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let f = || {
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                true,
                false,
            )]
        };
        {
            let mut d = driver();
            d.reconcile(f(), 0);
            let acts = d.due(60, |_, _| Gate::Go);
            let attempt = dispatches(&acts)[0].0.clone();
            d.answer(
                &label(),
                &attempt,
                Answer::Submitted {
                    started: Some("a1".into()),
                },
                61,
            );
        }
        append(&rollout, &[&line(STARTED, "a1"), &line(FAILED, "a1")]);
        let mut d = driver();
        assert!(
            d.reconcile(f(), 100)
                .iter()
                .all(|a| matches!(a, Act::Watch(_)))
        );
        let ep = load(dir.path()).unwrap();
        assert_eq!((ep.state, ep.failed_turn.as_str()), (State::Waiting, "a1"));
        let acts = d.due(ep.next_at.unwrap(), |_, _| Gate::Go);
        assert!(
            matches!(dispatches(&acts).as_slice(), [(a, "a1")] if a.n == 2),
            "{acts:?}"
        );
    }

    /// A turn whose start lies further back than the first window is still
    /// found: the window widens until it is.
    #[test]
    fn a_long_turn_is_read_back_to_its_start() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("events.jsonl");
        let filler = format!(r#"{{"type":"tool","out":"{}"}}"#, "x".repeat(1024));
        let mut lines = vec![r#"{"type":"turn_started","turn_number":4}"#.to_string()];
        lines.extend(std::iter::repeat_n(filler, 400));
        lines.push(r#"{"type":"turn_ended","outcome":"error"}"#.into());
        append(
            &events,
            &lines.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        assert!(std::fs::metadata(&events).unwrap().len() > BOOTSTRAP_WINDOWS[0]);
        let (_, _, s) = bootstrap(&events, Tool::Grok, "s1").unwrap();
        assert_eq!(
            s.latest().map(|t| (t.id.as_str(), &t.outcome)),
            Some(("4", &Outcome::Failed("unknown".into())))
        );
    }

    /// Nothing is dispatched unless its claim is on disk first, a disk
    /// that keeps failing is not retried in a tight loop, and once it works
    /// again the attempt is claimed and sent exactly once.
    #[test]
    fn a_claim_that_cannot_be_stored_dispatches_nothing_and_waits_to_retry() {
        enable(Tool::Codex, Some(Transport::CodexStart));
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let mut d = driver();
        d.reconcile(
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                true,
                false,
            )],
            0,
        );
        let blocked = episode_path(dir.path()).with_extension("json.tmp");
        std::fs::create_dir(&blocked).unwrap();
        assert!(d.due(60, |_, _| Gate::Go).is_empty());
        assert_eq!(d.next_due(), Some(60 + RETRY_LOOK), "held, not due at once");
        assert!(d.due(61, |_, _| Gate::Go).is_empty());
        assert_eq!(load(dir.path()).unwrap().attempts, [], "nothing claimed");

        std::fs::remove_dir(&blocked).unwrap();
        let acts = d.due(60 + RETRY_LOOK, |_, _| Gate::Go);
        assert_eq!(dispatches(&acts).len(), 1, "{acts:?}");
        assert_eq!(load(dir.path()).unwrap().attempts.len(), 1);
        assert!(dispatches(&d.due(60 + RETRY_LOOK, |_, _| Gate::Go)).is_empty());
    }

    /// A Codex rollout written after the agent was armed is found from the
    /// watch on its sessions root by its name, with no scan.
    #[test]
    fn a_rollout_created_after_arming_is_found_by_its_watch() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".codex/sessions");
        std::fs::create_dir_all(&root).unwrap();
        let mut d = Driver::new(PathBuf::from("/"), home.path().to_path_buf());
        let session: clank_core::agent_config::Session =
            serde_json::from_value(serde_json::json!({
                "id": "session-one", "tool": "codex", "updated_at": "2026-10-06T00:00:00Z",
            }))
            .unwrap();
        let f = Facts {
            author: label(),
            agent_dir: home.path().to_path_buf(),
            session,
            auto: true,
            delivering: false,
            obligated: true,
            stall_after: None,
            attending: false,
            asking: false,
        };
        let watch = WatchKey {
            dir: root.clone(),
            recursive: true,
        };
        assert_eq!(d.reconcile(vec![f.clone()], 0), [Act::Watch(watch.clone())]);
        assert!(
            d.reconcile(vec![f], 1).is_empty(),
            "a fold looks at nothing"
        );

        let day = root.join("2026/10/06");
        std::fs::create_dir_all(&day).unwrap();
        let other = day.join("rollout-2026-10-06T00-00-00-session-two.jsonl");
        append(&other, &[&line(STARTED, "x"), &line(FAILED, "x")]);
        let changed = |path: &Path| RecordEvent::Changed {
            watch: watch.clone(),
            path: path.to_path_buf(),
        };
        assert!(
            d.changed(changed(&other), 2).is_empty(),
            "another session's rollout"
        );
        let rollout = day.join("rollout-2026-10-06T00-00-01-session-one.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let acts = d.changed(changed(&rollout), 3);
        assert_eq!(notices(&acts).len(), 1, "{acts:?}");
        assert!(acts.contains(&Act::Unwatch(watch)), "{acts:?}");
        assert!(acts.contains(&Act::Watch(watch_of(&rollout))), "{acts:?}");
    }

    /// A fold reads nothing: a record that appears is read when its watch
    /// says so.
    #[test]
    fn a_record_is_read_on_its_watch_not_on_a_fold() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        let mut d = driver();
        let f = || {
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                true,
                false,
            )]
        };
        assert_eq!(d.reconcile(f(), 0), [Act::Watch(watch_of(&rollout))]);
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        assert!(d.reconcile(f(), 1).is_empty());
        assert_eq!(notices(&touched(&mut d, &rollout, 2)).len(), 1);
    }

    /// The same session's record moving to another file is followed.
    #[test]
    fn a_new_record_for_the_same_session_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.jsonl");
        let second = dir.path().join("second.jsonl");
        append(&first, &[&line(STARTED, "t1"), &line(DONE, "t1")]);
        append(&second, &[&line(STARTED, "t2"), &line(FAILED, "t2")]);
        let mut d = driver();
        assert!(
            notices(&d.reconcile(
                vec![codex_facts(dir.path(), &first, "session-one", true, false)],
                0
            ))
            .is_empty()
        );
        let acts = d.reconcile(
            vec![codex_facts(dir.path(), &second, "session-one", true, false)],
            1,
        );
        assert_eq!(notices(&acts).len(), 1, "{acts:?}");
    }

    /// Armed in the middle of a turn whose start is further back than the
    /// first window: the turn is read as running, and its end, when it
    /// comes, is that turn's failure.
    #[test]
    fn a_turn_running_when_armed_is_followed_to_its_end() {
        let dir = tempfile::tempdir().unwrap();
        let history = dir.path().join("chat_history.jsonl");
        let events = dir.path().join("events.jsonl");
        let filler = format!(r#"{{"type":"tool","out":"{}"}}"#, "x".repeat(1024));
        let mut lines = vec![r#"{"type":"turn_started","turn_number":4}"#.to_string()];
        lines.extend(std::iter::repeat_n(filler, 400));
        append(
            &events,
            &lines.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let (_, _, s) = bootstrap(&events, Tool::Grok, "session-one").unwrap();
        assert_eq!(
            s.latest().map(|t| (t.id.as_str(), &t.outcome)),
            Some(("4", &Outcome::Running))
        );
        let mut d = driver();
        let acts = d.reconcile(
            vec![facts(
                "grok",
                dir.path(),
                &history,
                "session-one",
                true,
                false,
            )],
            0,
        );
        assert_eq!(acts, [Act::Watch(watch_of(&events))]);
        append(&events, &[r#"{"type":"turn_ended","outcome":"error"}"#]);
        let acts = touched(&mut d, &events, 5);
        assert_eq!(notices(&acts).len(), 1, "{acts:?}");
    }

    /// A deadline whose record cannot be read dispatches nothing from what
    /// was read before, and a record that stays unreadable is told about
    /// rather than waited on forever.
    #[test]
    fn a_due_attempt_on_an_unreadable_record_dispatches_nothing() {
        enable(Tool::Codex, Some(Transport::CodexStart));
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let mut d = driver();
        d.reconcile(
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                true,
                false,
            )],
            0,
        );
        std::fs::remove_file(&rollout).unwrap();
        let mut now = 60;
        let mut told = Vec::new();
        for _ in 0..UNAVAILABLE_LIMIT {
            let acts = d.due(now, |_, _| Gate::Go);
            assert!(dispatches(&acts).is_empty(), "{acts:?}");
            told.extend(notices(&acts).into_iter().map(str::to_string));
            now += RETRY_LOOK;
        }
        assert!(
            matches!(told.as_slice(), [t] if t.contains("could not be read")),
            "{told:?}"
        );
        assert_eq!(load(dir.path()).unwrap().state, State::Escalated);
    }

    #[test]
    fn a_helpers_reply_is_applied_to_its_own_attempt_only() {
        enable(Tool::Codex, Some(Transport::Helper));
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let mut d = driver();
        d.reconcile(
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                true,
                false,
            )],
            0,
        );
        d.due(60, |_, _| Gate::Go);
        for (session, opened_by) in [("session-other", "t1"), ("session-one", "t0")] {
            write_reply(
                dir.path(),
                &Reply {
                    session: session.into(),
                    opened_by: opened_by.into(),
                    n: 1,
                    answer: ReplyAnswer::Submitted,
                    started: None,
                },
            )
            .unwrap();
            d.replies(61);
            assert_eq!(
                load(dir.path()).unwrap().attempts[0].disposition,
                Disposition::Claimed,
                "{session} {opened_by}"
            );
        }
        write_reply(
            dir.path(),
            &Reply {
                session: "session-one".into(),
                opened_by: "t1".into(),
                n: 1,
                answer: ReplyAnswer::Submitted,
                started: None,
            },
        )
        .unwrap();
        d.replies(62);
        assert_eq!(
            load(dir.path()).unwrap().attempts[0].disposition,
            Disposition::Submitted
        );
    }

    #[test]
    fn with_auto_mode_off_a_failure_is_only_told() {
        enable(Tool::Codex, Some(Transport::CodexStart));
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let mut d = driver();
        let mut f = codex_facts(dir.path(), &rollout, "session-one", true, false);
        f.auto = false;
        let acts = d.reconcile(vec![f], 0);
        assert!(
            matches!(notices(&acts).as_slice(), [t] if t.contains(AUTO_OFF)),
            "{acts:?}"
        );
        assert_eq!(d.next_due(), None, "nothing is scheduled");
    }

    /// A record replaced under the reader is read again from scratch, not
    /// appended to what it said before.
    #[test]
    fn a_replaced_record_is_read_afresh() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let mut d = driver();
        let acts = d.reconcile(
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                true,
                false,
            )],
            0,
        );
        assert_eq!(notices(&acts).len(), 1);
        std::fs::write(&rollout, format!("{}\n", line(STARTED, "t9"))).unwrap();
        touched(&mut d, &rollout, 10);
        assert_eq!(load(dir.path()).unwrap().state, State::Closed);
    }

    /// What a replaced record says is all there is: a turn it shows
    /// running is running, whatever the old record said of it.
    #[test]
    fn a_replaced_record_overrides_what_was_read_before() {
        enable(Tool::Codex, Some(Transport::CodexStart));
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let mut d = driver();
        d.reconcile(
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                true,
                false,
            )],
            0,
        );
        std::fs::write(&rollout, format!("{}\n", line(STARTED, "t1"))).unwrap();
        touched(&mut d, &rollout, 10);
        assert!(dispatches(&d.due(60, |_, _| Gate::Go)).is_empty());
    }

    /// An OpenCode agent's directory is watched before its plugin's first
    /// report creates the record, so that report is read.
    #[test]
    fn an_opencode_record_is_watched_before_its_first_report() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = driver();
        let record = observed_path(dir.path());
        let acts = d.reconcile(
            vec![facts(
                "opencode",
                dir.path(),
                &record,
                "session-one",
                true,
                false,
            )],
            0,
        );
        assert_eq!(acts, [Act::Watch(watch_of(&record))]);
        observe(
            dir.path(),
            &serde_json::json!({"session":"session-one","turn":"started","id":"m1"}),
        )
        .unwrap();
        observe(dir.path(), &serde_json::json!({"session":"session-zzz","turn":"failed","id":"m0","class":"billing_error"})).unwrap();
        assert!(
            touched(&mut d, &record, 1).is_empty(),
            "another session's failure"
        );
        observe(dir.path(), &serde_json::json!({"session":"session-one","turn":"failed","id":"m1","class":"billing_error"})).unwrap();
        let acts = touched(&mut d, &record, 2);
        assert_eq!(notices(&acts).len(), 1, "{acts:?}");
    }

    #[test]
    fn only_an_escalated_episode_is_waiting_for_a_person() {
        let dir = tempfile::tempdir().unwrap();
        let s = snap([started("t1"), failed("t1", "server_error")]);
        let waiting = decide(None, &s, &ctx(0, HELPER, None)).0;
        store(dir.path(), waiting.as_ref()).unwrap();
        assert_eq!(waiting_for_you(dir.path()), None, "still being retried");
        let s = snap([started("t1"), failed("t1", "billing_error")]);
        let escalated = decide(None, &s, &ctx(0, HELPER, None)).0;
        store(dir.path(), escalated.as_ref()).unwrap();
        assert!(waiting_for_you(dir.path()).is_some_and(|t| t.contains("billing_error")));
    }

    #[test]
    fn opencode_errors_map_to_the_classes_retries_know() {
        assert_eq!(opencode_class("APIError", Some(529)), "overloaded");
        assert_eq!(opencode_class("APIError", Some(500)), "server_error");
        assert_eq!(opencode_class("APIError", Some(429)), "rate_limit");
        assert_eq!(
            opencode_class("ProviderAuthError", None),
            "authentication_failed"
        );
        assert_eq!(
            opencode_class("MessageOutputLengthError", None),
            "max_output_tokens"
        );
        assert_eq!(opencode_class("UnknownError", None), "unknown");
    }

    #[test]
    fn an_episode_survives_the_disk_and_a_reply_is_taken_once() {
        let dir = tempfile::tempdir().unwrap();
        let s = snap([started("t1"), failed("t1", "server_error")]);
        let ep = decide(None, &s, &ctx(0, HELPER, None)).0;
        store(dir.path(), ep.as_ref()).unwrap();
        assert_eq!(load(dir.path()), ep);
        store(dir.path(), None).unwrap();
        assert_eq!(load(dir.path()), None);
        store(dir.path(), None).unwrap();

        let reply = Reply {
            session: "s1".into(),
            opened_by: "t1".into(),
            n: 1,
            answer: ReplyAnswer::Submitted,
            started: Some("m1".into()),
        };
        write_reply(dir.path(), &reply).unwrap();
        assert_eq!(take_reply(dir.path()), Some(reply));
        assert_eq!(take_reply(dir.path()), None);
    }

    fn poll_backend(
        handler: Box<dyn FnMut(notify::Result<notify::Event>) + Send>,
    ) -> notify::Result<Backend> {
        Ok(Box::new(notify::PollWatcher::new(
            handler,
            // Its mtimes are whole seconds; contents see every append.
            notify::Config::default()
                .with_poll_interval(std::time::Duration::from_millis(20))
                .with_compare_contents(true),
        )?) as Backend)
    }

    /// A watch reports itself once set up, so what changed while it was
    /// being set up is read, then each change under it, by the path that
    /// changed.
    #[tokio::test]
    async fn a_watch_reports_what_changes_under_it() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("rollout.jsonl");
        append(&record, &["{}"]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let watch = RecordWatch::spawn_with(tx, poll_backend);
        let key = watch_of(&record);
        watch.watch(key.clone());
        let within = std::time::Duration::from_secs(10);
        let mut next = async || {
            tokio::time::timeout(within, rx.recv())
                .await
                .unwrap()
                .unwrap()
        };
        assert_eq!(
            next().await,
            RecordEvent::Changed {
                watch: key.clone(),
                path: key.dir.clone()
            }
        );
        append(&record, &["{}", "{}"]);
        assert_eq!(
            next().await,
            RecordEvent::Changed {
                watch: key.clone(),
                path: record.clone()
            }
        );

        watch.unwatch(key);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        while rx.try_recv().is_ok() {}
        append(&record, &["{}"]);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(rx.try_recv().is_err(), "an unwatched record is quiet");
    }

    /// A backend the test drives: it counts installs, can refuse one, can
    /// act while one is being set up, and can fail an installed watch.
    type Shared<T> = std::sync::Arc<std::sync::Mutex<T>>;
    type Handler = Box<dyn FnMut(notify::Result<notify::Event>) + Send>;
    type During = Box<dyn FnMut(&Path) + Send>;

    #[derive(Clone, Default)]
    struct Fake {
        installs: Shared<Vec<PathBuf>>,
        refuse: Shared<Option<PathBuf>>,
        during: Shared<Option<During>>,
        handler: Shared<Option<Handler>>,
    }

    struct FakeBackend(Fake);

    impl notify::Watcher for FakeBackend {
        fn new<F: notify::EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
            Err(notify::Error::generic("made by Fake::spawn"))
        }
        fn watch(&mut self, path: &Path, _: notify::RecursiveMode) -> notify::Result<()> {
            if let Some(during) = self.0.during.lock().unwrap().as_mut() {
                during(path);
            }
            if self.0.refuse.lock().unwrap().as_deref() == Some(path) {
                return Err(notify::Error::path_not_found().add_path(path.to_path_buf()));
            }
            self.0.installs.lock().unwrap().push(path.to_path_buf());
            Ok(())
        }
        fn unwatch(&mut self, _: &Path) -> notify::Result<()> {
            Ok(())
        }
        fn kind() -> notify::WatcherKind {
            notify::WatcherKind::NullWatcher
        }
    }

    impl Fake {
        fn spawn(&self, tx: tokio::sync::mpsc::UnboundedSender<RecordEvent>) -> RecordWatch {
            let fake = self.clone();
            RecordWatch::spawn_with(tx, move |handler| {
                *fake.handler.lock().unwrap() = Some(handler);
                Ok(Box::new(FakeBackend(fake)) as Backend)
            })
        }

        fn lose(&self, dir: &Path) {
            let err = notify::Error::generic("backend lost it").add_path(dir.to_path_buf());
            (self.handler.lock().unwrap().as_mut().unwrap())(Err(err));
        }

        fn installs(&self) -> usize {
            self.installs.lock().unwrap().len()
        }
    }

    fn forward(watch: &RecordWatch, acts: &[Act]) {
        for act in acts {
            match act {
                Act::Watch(k) => watch.watch(k.clone()),
                Act::Unwatch(k) => watch.unwatch(k.clone()),
                _ => {}
            }
        }
    }

    async fn next_event(rx: &mut tokio::sync::mpsc::UnboundedReceiver<RecordEvent>) -> RecordEvent {
        tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn a_watch_that_cannot_be_installed_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        *fake.refuse.lock().unwrap() = Some(dir.path().to_path_buf());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let watch = fake.spawn(tx);
        let key = WatchKey {
            dir: dir.path().to_path_buf(),
            recursive: false,
        };
        watch.watch(key.clone());
        assert_eq!(
            next_event(&mut rx).await,
            RecordEvent::Failed { watch: key }
        );
    }

    /// A watch the backend loses after it was installed is installed again,
    /// once, by the next fold, for every agent sharing it, and reports
    /// itself so what changed meanwhile is read. Not before that fold: a
    /// backend that keeps failing is not retried in a loop.
    #[tokio::test]
    async fn a_lost_watch_is_installed_again_once_for_all_who_share_it() {
        let dir = tempfile::tempdir().unwrap();
        let (one, two) = (dir.path().join("one.jsonl"), dir.path().join("two.jsonl"));
        append(&one, &[&line(STARTED, "a")]);
        append(&two, &[&line(STARTED, "b")]);
        let agent = |name: &str, record: &Path| Facts {
            author: clank_core::ids::AgentLabel::parse(name).unwrap(),
            ..codex_facts(&dir.path().join(name), record, "session-one", true, false)
        };
        for name in ["first", "second"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        let both = || vec![agent("first", &one), agent("second", &two)];
        let fake = Fake::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let watch = fake.spawn(tx);
        let mut d = driver();
        let key = watch_of(&one);

        let acts = d.reconcile(both(), 0);
        assert_eq!(acts, [Act::Watch(key.clone())], "one watch for both");
        forward(&watch, &acts);
        let setup = next_event(&mut rx).await;
        assert!(d.changed(setup, 1).is_empty());
        assert_eq!(fake.installs(), 1);

        fake.lose(&key.dir);
        let lost = next_event(&mut rx).await;
        assert_eq!(lost, RecordEvent::Failed { watch: key.clone() });
        assert!(d.changed(lost, 2).is_empty(), "not again until a fold");

        append(&one, &[&line(FAILED, "a")]);
        let acts = d.reconcile(both(), 3);
        assert_eq!(acts, [Act::Watch(key.clone())], "installed again, once");
        forward(&watch, &acts);
        let setup = next_event(&mut rx).await;
        assert_eq!(
            setup,
            RecordEvent::Changed {
                watch: key.clone(),
                path: key.dir.clone()
            }
        );
        assert_eq!(fake.installs(), 2);
        let acts = d.changed(setup, 4);
        assert_eq!(
            notices(&acts).len(),
            1,
            "what changed meanwhile is read: {acts:?}"
        );

        assert!(
            d.reconcile(vec![agent("second", &two)], 5).is_empty(),
            "still shared"
        );
        assert_eq!(d.reconcile(vec![], 6), [Act::Unwatch(key)]);
    }

    /// A Codex rollout written and gone quiet while its sessions root was
    /// still being watched for is found when that watch comes up.
    #[tokio::test]
    async fn a_rollout_written_while_its_watch_was_set_up_is_found() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".codex/sessions");
        std::fs::create_dir_all(&root).unwrap();
        let rollout = root.join("2026/10/06/rollout-2026-10-06T00-00-01-session-one.jsonl");
        let fake = Fake::default();
        let written = rollout.clone();
        *fake.during.lock().unwrap() = Some(Box::new(move |_: &Path| {
            std::fs::create_dir_all(written.parent().unwrap()).unwrap();
            append(&written, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        }));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let watch = fake.spawn(tx);
        let mut d = Driver::new(PathBuf::from("/"), home.path().to_path_buf());
        let session: clank_core::agent_config::Session =
            serde_json::from_value(serde_json::json!({
                "id": "session-one", "tool": "codex", "updated_at": "2026-10-06T00:00:00Z",
            }))
            .unwrap();
        let f = Facts {
            author: label(),
            agent_dir: home.path().to_path_buf(),
            session,
            auto: true,
            delivering: false,
            obligated: true,
            stall_after: None,
            attending: false,
            asking: false,
        };
        let acts = d.reconcile(vec![f], 0);
        forward(&watch, &acts);
        let setup = next_event(&mut rx).await;
        let acts = d.changed(setup, 1);
        assert_eq!(notices(&acts).len(), 1, "{acts:?}");
        assert!(acts.contains(&Act::Watch(watch_of(&rollout))), "{acts:?}");
    }

    fn block_storage(dir: &Path) -> PathBuf {
        let blocked = episode_path(dir).with_extension("json.tmp");
        std::fs::create_dir(&blocked).unwrap();
        blocked
    }

    /// While storage is held, nothing tries to write: not a record event,
    /// not an answer. What they brought is kept for the decision at the
    /// hold's end.
    #[test]
    fn nothing_writes_during_a_storage_hold_and_nothing_is_lost() {
        enable(Tool::Codex, Some(Transport::Helper));
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let mut d = driver();
        d.reconcile(
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                true,
                false,
            )],
            0,
        );
        let acts = d.due(60, |_, _| Gate::Go);
        let attempt = dispatches(&acts)[0].0.clone();
        let blocked = block_storage(dir.path());
        assert!(
            d.answer(&label(), &attempt, Answer::Submitted { started: None }, 61)
                .is_empty()
        );
        let held = d.next_due();
        assert_eq!(held, Some(61 + RETRY_LOOK));
        assert!(touched(&mut d, &rollout, 62).is_empty());
        assert_eq!(d.next_due(), held, "an event during the hold wrote nothing");

        std::fs::remove_dir(&blocked).unwrap();
        d.due(61 + RETRY_LOOK, |_, _| Gate::Go);
        assert_eq!(
            load(dir.path()).unwrap().attempts[0].disposition,
            Disposition::Submitted,
            "the answer from the hold was applied"
        );
    }

    /// A notice that could not be stored is not lost: its decision is owed,
    /// with a deadline of its own, and is made once storage works.
    #[test]
    fn a_notice_that_could_not_be_stored_is_retried_after_the_hold() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let blocked = block_storage(dir.path());
        let mut d = driver();
        let acts = d.reconcile(
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                true,
                false,
            )],
            0,
        );
        assert!(notices(&acts).is_empty());
        assert_eq!(d.next_due(), Some(RETRY_LOOK));
        std::fs::remove_dir(&blocked).unwrap();
        let acts = d.due(RETRY_LOOK, |_, _| Gate::Go);
        assert_eq!(notices(&acts).len(), 1, "{acts:?}");
        assert_eq!(d.next_due(), None);
        assert!(
            d.due(RETRY_LOOK * 2, |_, _| Gate::Go).is_empty(),
            "told once"
        );
    }

    fn notices_with_keys(acts: &[Act]) -> Vec<(&str, &str)> {
        acts.iter()
            .filter_map(|a| match a {
                Act::Notice { key, text, .. } => Some((key.as_str(), text.as_str())),
                _ => None,
            })
            .collect()
    }

    /// An uncertain answer held across a storage failure, overtaken by the
    /// agent finishing a turn: what is stored and told is the finish, not
    /// the escalation the answer alone would have meant.
    #[test]
    fn a_held_answer_overtaken_by_recovery_tells_nobody() {
        enable(Tool::Codex, Some(Transport::Helper));
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let mut d = driver();
        d.reconcile(
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                true,
                false,
            )],
            0,
        );
        let attempt = dispatches(&d.due(60, |_, _| Gate::Go))[0].0.clone();
        let blocked = block_storage(dir.path());
        assert!(
            d.answer(&label(), &attempt, Answer::Indeterminate, 61)
                .is_empty()
        );
        append(&rollout, &[&line(STARTED, "t2"), &line(DONE, "t2")]);
        assert!(touched(&mut d, &rollout, 62).is_empty());

        std::fs::remove_dir(&blocked).unwrap();
        let acts = d.due(61 + RETRY_LOOK, |_, _| Gate::Go);
        assert_eq!(notices_with_keys(&acts), [], "{acts:?}");
        let ep = load(dir.path()).unwrap();
        assert_eq!(ep.state, State::Closed);
        assert_eq!(ep.attempts[0].disposition, Disposition::Indeterminate);
    }

    /// A held answer for one episode, then a person's turn that fails
    /// differently: the stored episode and its notice are the new
    /// failure's, under its own key, and the old one is told nothing.
    #[test]
    fn a_held_answer_overtaken_by_a_new_failure_tells_only_the_new_one() {
        enable(Tool::Codex, Some(Transport::Helper));
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let mut d = driver();
        d.reconcile(
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                true,
                false,
            )],
            0,
        );
        let attempt = dispatches(&d.due(60, |_, _| Gate::Go))[0].0.clone();
        let blocked = block_storage(dir.path());
        let started = Answer::Submitted {
            started: Some("a1".into()),
        };
        assert!(d.answer(&label(), &attempt, started, 61).is_empty());
        let billing = FAILED.replace("server_overloaded", "billing_error");
        append(&rollout, &[&line(STARTED, "p1"), &line(&billing, "p1")]);
        assert!(touched(&mut d, &rollout, 62).is_empty());

        std::fs::remove_dir(&blocked).unwrap();
        let acts = d.due(61 + RETRY_LOOK, |_, _| Gate::Go);
        let told = notices_with_keys(&acts);
        assert!(
            matches!(told.as_slice(), [("session-one:p1", text)] if text.contains("billing_error")),
            "{acts:?}"
        );
        let ep = load(dir.path()).unwrap();
        assert_eq!((ep.opened_by.as_str(), ep.state), ("p1", State::Escalated));
    }

    /// `prompt` of session-one asks a question, the session bound as the
    /// question hook requires.
    fn ask(dir: &Path, prompt: &str) {
        std::fs::write(
            dir.join("config.json"),
            serde_json::json!({"session": {
                "id": "session-one", "tool": "claude", "updated_at": "2026-10-05T00:00:00Z",
            }})
            .to_string(),
        )
        .unwrap();
        crate::cli::question_state::observe(
            dir,
            [crate::cli::question_state::Observation::Asked {
                key: crate::cli::question_state::Key {
                    session: "session-one".into(),
                    prompt: prompt.into(),
                    tool_use: "t".into(),
                },
                header: "Which?".into(),
                input: "h".into(),
            }],
        );
        assert!(asking(dir), "asked");
    }

    fn asking(dir: &Path) -> bool {
        crate::cli::question_state::asking(dir, "session-one").is_some()
    }

    /// A question whose turn failed on a provider error (no result hook, no
    /// Stop, the binding unchanged) is over once the record is read, and so
    /// is one a newer prompt has left behind; one still waiting is not.
    #[test]
    fn the_record_ends_questions_its_turns_have_left() {
        let error = r#"{"type":"assistant","isApiErrorMessage":true,"error":"overloaded"}"#;
        for (lines, still) in [
            (vec![user("p1")], true),
            (vec![user("p1"), error.to_string()], false),
            (vec![user("p1"), user("p2")], false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let transcript = dir.path().join("t.jsonl");
            append(
                &transcript,
                &lines.iter().map(String::as_str).collect::<Vec<_>>(),
            );
            ask(dir.path(), "p1");
            let mut d = driver();
            d.reconcile(
                vec![facts(
                    "claude",
                    dir.path(),
                    &transcript,
                    "session-one",
                    true,
                    false,
                )],
                0,
            );
            assert_eq!(asking(dir.path()), still, "{lines:?}");
        }
    }

    /// A question still waiting outlives a restart: no hook is needed to
    /// keep it, and nothing in the record ends it.
    #[test]
    fn a_waiting_question_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        append(
            &transcript,
            &[r#"{"type":"user","promptId":"p1","message":{"content":"go"}}"#],
        );
        ask(dir.path(), "p1");
        for _ in 0..2 {
            let mut d = driver();
            d.reconcile(
                vec![facts(
                    "claude",
                    dir.path(),
                    &transcript,
                    "session-one",
                    true,
                    false,
                )],
                0,
            );
            assert!(asking(dir.path()));
        }
    }

    fn user(id: &str) -> String {
        format!(r#"{{"type":"user","promptId":"{id}","message":{{"content":"go"}}}}"#)
    }

    /// An idle Claude agent, owing nothing, with a question pending.
    fn claude_agent(dir: &Path, transcript: &Path) -> Driver {
        let mut d = driver();
        let acts = d.reconcile(
            vec![Facts {
                asking: true,
                ..facts("claude", dir, transcript, "session-one", false, false)
            }],
            0,
        );
        assert!(
            acts.iter().all(|a| matches!(a, Act::Watch(_))),
            "nothing is decided for it: {acts:?}"
        );
        d
    }

    /// A restart whose first read starts after the question's prompt, and
    /// after the Stop hook's stretch too, still finds the newer prompt
    /// that left it behind.
    #[test]
    fn a_restart_ends_a_question_older_than_what_it_reads() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        let filler = format!(r#"{{"type":"system","pad":"{}"}}"#, "x".repeat(1024));
        append(&transcript, &[&user("p1")]);
        append(&transcript, &vec![filler.as_str(); 300]);
        append(&transcript, &[&user("p2")]);
        ask(dir.path(), "p1");
        let (_, _, snap) = bootstrap(&transcript, Tool::Claude, "session-one").unwrap();
        assert_eq!(snap.ids().collect::<Vec<_>>(), vec!["p2"], "p1 is not read");
        claude_agent(dir.path(), &transcript);
        assert!(!asking(dir.path()));
    }

    /// A question's prompt published, then left behind by more turns than
    /// the snapshot keeps, all within one read, is still behind: the hook
    /// ran before the record held the prompt, so the read is the first to
    /// see it.
    #[test]
    fn a_long_read_ends_a_question_its_snapshot_never_held() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        append(&transcript, &[&user("p0")]);
        ask(dir.path(), "p1");
        let mut d = claude_agent(dir.path(), &transcript);
        assert!(asking(dir.path()));
        let mut later = vec![user("p1")];
        later.extend((0..SNAPSHOT_TURNS + 2).map(|i| user(&format!("q{i}"))));
        append(
            &transcript,
            &later.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        touched(&mut d, &transcript, 1);
        assert!(!asking(dir.path()));
    }

    /// A question cancelled in the terminal has no result hook and no
    /// Stop, only its result in the record: that ends it, read live or
    /// found on a restart.
    #[test]
    fn a_cancelled_question_ends_on_its_recorded_result() {
        let rejected = r#"{"type":"user","promptId":"p1","message":{"content":[{"type":"tool_result","tool_use_id":"t","is_error":true}]}}"#;
        for restart in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let transcript = dir.path().join("t.jsonl");
            append(&transcript, &[&user("p1")]);
            ask(dir.path(), "p1");
            if restart {
                append(&transcript, &[rejected]);
                claude_agent(dir.path(), &transcript);
            } else {
                let mut d = claude_agent(dir.path(), &transcript);
                assert!(asking(dir.path()));
                append(&transcript, &[rejected]);
                touched(&mut d, &transcript, 1);
            }
            assert!(!asking(dir.path()), "restart: {restart}");
        }
    }

    /// An agent read only for its question is never recovered from what
    /// its record shows: a failed turn it owes nothing for is not acted on.
    #[test]
    fn a_record_read_for_a_question_decides_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        append(&transcript, &[&user("p1")]);
        ask(dir.path(), "p1");
        let mut d = claude_agent(dir.path(), &transcript);
        append(
            &transcript,
            &[
                r#"{"type":"assistant","promptId":"p1","isApiErrorMessage":true,"error":"overloaded"}"#,
            ],
        );
        let acts = touched(&mut d, &transcript, 1);
        assert!(acts.is_empty(), "{acts:?}");
        assert!(d.due(10_000, |_, _| Gate::Go).is_empty());
        assert!(
            !asking(dir.path()),
            "the failed turn still ends its question"
        );
    }

    const AFTER: u64 = 40 * 60;

    fn stalling(dir: &Path, record: &Path) -> Facts {
        Facts {
            stall_after: Some(AFTER),
            ..codex_facts(dir, record, "session-one", true, false)
        }
    }

    /// A rollout whose latest turn finished, and an agent still owing work.
    fn quiet_agent() -> (tempfile::TempDir, PathBuf, Driver) {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(DONE, "t1")]);
        let mut d = driver();
        assert!(
            d.reconcile(vec![stalling(dir.path(), &rollout)], 0)
                .iter()
                .all(|a| matches!(a, Act::Watch(_)))
        );
        (dir, rollout, d)
    }

    const NOW_GO: fn(&clank_core::ids::AgentLabel, &str) -> Gate = |_, _| Gate::Go;

    /// Pending work, nothing waiting for it, and a record that has not
    /// moved: one notice at the configured interval, kept for status, and
    /// nothing more until eligibility resets.
    #[test]
    fn a_stalled_agent_is_told_once() {
        let (dir, _, mut d) = quiet_agent();
        assert_eq!(d.next_due(), Some(AFTER), "one deadline, nothing else");
        assert!(d.due(AFTER - 1, NOW_GO).is_empty());
        let acts = d.due(AFTER, NOW_GO);
        assert_eq!(
            notices_with_keys(&acts),
            [(
                "session-one:stall:0",
                "agent has had pending work for 40m and is not waiting for it"
            )]
        );
        assert_eq!(
            waiting_for_you(dir.path()).as_deref(),
            Some("agent has had pending work for 40m and is not waiting for it")
        );
        assert_eq!(d.next_due(), None);
        assert!(d.due(AFTER * 3, NOW_GO).is_empty());
    }

    /// A slow tool writes to the record without any turn ending: the
    /// deadline is re-armed from the last movement, not told.
    #[test]
    fn a_record_that_moved_before_the_deadline_is_not_stalled() {
        let (_dir, rollout, mut d) = quiet_agent();
        append(
            &rollout,
            &[r#"{"type":"response_item","payload":{"type":"function_call_output"}}"#],
        );
        assert!(notices(&d.due(AFTER, NOW_GO)).is_empty());
        assert_eq!(
            d.next_due(),
            Some(AFTER * 2),
            "counted from the movement it found"
        );
        assert_eq!(notices(&d.due(AFTER * 2, NOW_GO)).len(), 1);
    }

    /// A person stopped the latest turn: nothing is told, and nothing
    /// waits, until the record moves again.
    #[test]
    fn a_stopped_turn_is_not_a_stall_until_the_record_moves() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        let aborted = r#"{"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"T","reason":"interrupted"}}"#;
        append(&rollout, &[&line(STARTED, "t1"), &line(aborted, "t1")]);
        let mut d = driver();
        d.reconcile(vec![stalling(dir.path(), &rollout)], 0);
        assert!(d.due(AFTER, NOW_GO).is_empty());
        assert_eq!(d.next_due(), None, "nothing waits on a stopped turn");
        append(&rollout, &[&line(STARTED, "t2")]);
        touched(&mut d, &rollout, AFTER + 5);
        assert_eq!(d.next_due(), Some(AFTER + 5 + AFTER));
    }

    /// A failure the record shows is failed-turn recovery's: it is told
    /// once, by that, and never again as a stall.
    #[test]
    fn a_recorded_failure_is_left_to_failed_turn_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let mut d = driver();
        let acts = d.reconcile(vec![stalling(dir.path(), &rollout)], 0);
        assert!(matches!(notices(&acts).as_slice(), [t] if t.contains("server_overloaded")));
        assert!(notices(&d.due(AFTER, NOW_GO)).is_empty());
        assert_eq!(load_stall(dir.path()).unwrap().state, StallState::Quiet);
    }

    /// A deliverer, the end of the obligation or of auto mode means no
    /// deadline at all; attendance or a block at the deadline counts it
    /// again from there. Each just before the deadline cancels it.
    #[test]
    fn eligibility_ending_before_the_deadline_cancels_it() {
        for end in ["deliverer", "obligation", "auto"] {
            let (dir, rollout, mut d) = quiet_agent();
            let mut f = stalling(dir.path(), &rollout);
            match end {
                "deliverer" => f.delivering = true,
                "obligation" => f.obligated = false,
                _ => f.auto = false,
            }
            d.reconcile(vec![f], AFTER - 1);
            assert_eq!(load_stall(dir.path()), None, "{end}");
            assert!(d.due(AFTER, NOW_GO).is_empty(), "{end}");
            assert_eq!(d.next_due(), None, "{end}");
        }
        let (_dir, _, mut d) = quiet_agent();
        assert!(d.due(AFTER, |_, _| Gate::Hold).is_empty());
        assert_eq!(d.next_due(), Some(AFTER * 2), "attending: counted again");
        for gone in [Gate::Deliverer, Gate::SessionChanged, Gate::AutoOff] {
            let (dir, _, mut d) = quiet_agent();
            assert!(d.due(AFTER, |_, _| gone).is_empty(), "{gone:?}");
            assert_eq!(load_stall(dir.path()), None, "{gone:?}");
        }
    }

    /// `off` disables only this: no deadline, while a recorded failure is
    /// still told.
    #[test]
    fn off_disables_the_stall_and_nothing_else() {
        let (dir, rollout, mut d) = quiet_agent();
        let f = || Facts {
            stall_after: None,
            ..stalling(dir.path(), &rollout)
        };
        d.reconcile(vec![f()], 1);
        assert_eq!((d.next_due(), load_stall(dir.path())), (None, None));
        append(&rollout, &[&line(STARTED, "t2"), &line(FAILED, "t2")]);
        assert_eq!(notices(&touched(&mut d, &rollout, 2)).len(), 1);
    }

    /// A monitor restarted before the deadline keeps it, one restarted
    /// after it fires it once, and one restarted after the notice does not
    /// tell again.
    #[test]
    fn a_restart_keeps_the_deadline_and_never_tells_twice() {
        let (dir, rollout, d) = quiet_agent();
        drop(d);
        let mut d = driver();
        d.reconcile(vec![stalling(dir.path(), &rollout)], 100);
        assert_eq!(
            d.next_due(),
            Some(AFTER),
            "the deadline it had, not a new one"
        );
        drop(d);
        let mut d = driver();
        d.reconcile(vec![stalling(dir.path(), &rollout)], AFTER + 100);
        assert_eq!(notices(&d.due(AFTER + 100, NOW_GO)).len(), 1);
        drop(d);
        let mut d = driver();
        let acts = d.reconcile(vec![stalling(dir.path(), &rollout)], AFTER + 200);
        assert!(notices(&acts).is_empty());
        assert!(d.due(AFTER * 5, NOW_GO).is_empty());
    }

    /// A record clank cannot find, or cannot make sense of, is no reason
    /// to stay quiet: that is what this fallback is for.
    #[test]
    fn a_missing_or_unreadable_record_is_still_told() {
        for record in ["missing", "garbage"] {
            let dir = tempfile::tempdir().unwrap();
            let rollout = dir.path().join("rollout.jsonl");
            if record == "garbage" {
                append(&rollout, &["not json", "{\"type\":\"something else\"}"]);
            }
            let mut d = driver();
            d.reconcile(vec![stalling(dir.path(), &rollout)], 0);
            assert_eq!(notices(&d.due(AFTER, NOW_GO)).len(), 1, "{record}");
        }
    }

    /// A notice that could not be stored is held, not retried in a loop,
    /// and told once storage works.
    #[test]
    fn a_stall_that_could_not_be_stored_is_told_after_the_hold() {
        let (dir, _, mut d) = quiet_agent();
        let blocked = stall_path(dir.path()).with_extension("json.tmp");
        std::fs::create_dir(&blocked).unwrap();
        assert!(d.due(AFTER, NOW_GO).is_empty());
        assert_eq!(d.next_due(), Some(AFTER + RETRY_LOOK));
        assert!(d.due(AFTER + 1, NOW_GO).is_empty());
        std::fs::remove_dir(&blocked).unwrap();
        assert_eq!(notices(&d.due(AFTER + RETRY_LOOK, NOW_GO)).len(), 1);
    }

    /// A failure whose recovery already ended (here, its retry was
    /// refused) is still the failed-turn plan's: the stall stays quiet.
    #[test]
    fn a_failure_whose_recovery_ended_is_still_not_a_stall() {
        enable(Tool::Codex, Some(Transport::Helper));
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let mut d = driver();
        d.reconcile(vec![stalling(dir.path(), &rollout)], 0);
        let attempt = dispatches(&d.due(60, NOW_GO))[0].0.clone();
        d.answer(&label(), &attempt, Answer::Refused, 61);
        assert_eq!(load(dir.path()).unwrap().state, State::Closed);
        assert!(notices(&d.due(AFTER, NOW_GO)).is_empty());
        assert_eq!(load_stall(dir.path()).unwrap().state, StallState::Quiet);
    }

    /// During a storage hold a fold writes nothing either: the stall a
    /// deliverer ends is let go when the hold does.
    #[test]
    fn a_stall_hold_covers_folds_too() {
        let (dir, rollout, mut d) = quiet_agent();
        let blocked = stall_path(dir.path()).with_extension("json.tmp");
        std::fs::create_dir(&blocked).unwrap();
        d.due(AFTER, NOW_GO);
        let held = d.next_due();
        let delivering = || Facts {
            delivering: true,
            ..stalling(dir.path(), &rollout)
        };
        d.reconcile(vec![delivering()], AFTER + 1);
        assert_eq!(d.next_due(), held, "nothing was written");
        assert!(load_stall(dir.path()).is_some());
        std::fs::remove_dir(&blocked).unwrap();
        d.reconcile(vec![delivering()], AFTER + RETRY_LOOK);
        assert_eq!(load_stall(dir.path()), None);
    }

    /// A stall that could not be stored, whether its first arming, a
    /// clear, or a policy that changed during the hold, is decided again
    /// from the agent as it is when the hold ends, and a disk that keeps
    /// failing moves the hold on rather than leaving it due.
    #[test]
    fn a_stall_decision_owed_across_a_hold_is_rebuilt_from_now() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(DONE, "t1")]);
        let blocked = stall_path(dir.path()).with_extension("json.tmp");
        std::fs::create_dir(&blocked).unwrap();
        let mut d = driver();
        d.reconcile(vec![stalling(dir.path(), &rollout)], 0);
        assert_eq!(d.next_due(), Some(RETRY_LOOK), "first arming owed");
        d.due(RETRY_LOOK, NOW_GO);
        assert_eq!(
            d.next_due(),
            Some(RETRY_LOOK * 2),
            "still failing: moved on"
        );
        std::fs::remove_dir(&blocked).unwrap();
        d.due(RETRY_LOOK * 2, NOW_GO);
        let armed = load_stall(dir.path()).unwrap();
        assert_eq!(
            (armed.state, armed.since),
            (StallState::Waiting, 0),
            "counted from when it became eligible, not from the save"
        );
        assert_eq!(d.next_due(), Some(AFTER));

        // A clear that fails, then `off` during the hold.
        std::fs::remove_file(stall_path(dir.path())).unwrap();
        std::fs::create_dir(stall_path(dir.path())).unwrap();
        let off = || Facts {
            stall_after: None,
            ..stalling(dir.path(), &rollout)
        };
        d.reconcile(
            vec![Facts {
                delivering: true,
                ..stalling(dir.path(), &rollout)
            }],
            1000,
        );
        assert_eq!(d.next_due(), Some(1000 + RETRY_LOOK));
        d.reconcile(vec![off()], 1001);
        std::fs::remove_dir(stall_path(dir.path())).unwrap();
        assert!(d.due(1000 + RETRY_LOOK, NOW_GO).is_empty());
        assert_eq!(
            (d.next_due(), load_stall(dir.path())),
            (None, None),
            "off, as it is now"
        );
    }

    fn stop(turn: &str) -> String {
        line(
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"T","reason":"interrupted"}}"#,
            turn,
        )
    }

    /// A record replaced by another of the same size, or a session whose
    /// record moved to another path, is progress: a quiet stall counts
    /// again from there.
    #[test]
    fn a_replaced_record_of_the_same_size_is_progress() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &stop("t1")]);
        let size = std::fs::metadata(&rollout).unwrap().len() as usize;
        let mut d = driver();
        d.reconcile(vec![stalling(dir.path(), &rollout)], 0);
        d.due(AFTER, NOW_GO);
        assert_eq!(load_stall(dir.path()).unwrap().state, StallState::Quiet);

        let running = format!("{}\n", line(STARTED, "t2"));
        let pad = size - running.len() - 1;
        let filler = format!(r#"{{"pad":"{}"}}"#, "x".repeat(pad - 10));
        let replacement = format!("{running}{filler}\n");
        assert_eq!(replacement.len(), size);
        let tmp = dir.path().join("rollout.new");
        std::fs::write(&tmp, replacement).unwrap();
        std::fs::rename(&tmp, &rollout).unwrap();
        touched(&mut d, &rollout, AFTER + 10);
        let s = load_stall(dir.path()).unwrap();
        assert_eq!((s.state, s.since), (StallState::Waiting, AFTER + 10));

        let moved = dir.path().join("moved.jsonl");
        append(&moved, &[&line(STARTED, "t3"), &stop("t3")]);
        d.due(AFTER * 2 + 10, NOW_GO);
        d.reconcile(vec![stalling(dir.path(), &moved)], AFTER * 3);
        let s = load_stall(dir.path()).unwrap();
        assert_eq!(
            (s.state, s.since),
            (StallState::Waiting, AFTER * 3),
            "another record"
        );
    }

    /// A question, block or proven attendance ends eligibility like
    /// anything else: no deadline while it lasts, a told notice let go, and
    /// a fresh interval, and a fresh notice, once it is over.
    #[test]
    fn attending_ends_the_stall_and_resuming_starts_it_afresh() {
        let (dir, rollout, mut d) = quiet_agent();
        let attending = |on: bool| Facts {
            attending: on,
            ..stalling(dir.path(), &rollout)
        };
        d.reconcile(vec![attending(true)], AFTER - 10);
        assert_eq!((d.next_due(), load_stall(dir.path())), (None, None));
        d.reconcile(vec![attending(false)], AFTER - 5);
        assert_eq!(d.next_due(), Some(AFTER - 5 + AFTER), "a fresh interval");
        let told = d.due(AFTER * 2 - 5, NOW_GO);
        assert_eq!(notices(&told).len(), 1);

        d.reconcile(vec![attending(true)], AFTER * 3);
        assert_eq!(waiting_for_you(dir.path()), None, "the notice is let go");
        d.reconcile(vec![attending(false)], AFTER * 4);
        let again = d.due(AFTER * 5, NOW_GO);
        assert_eq!(
            notices_with_keys(&again).first().map(|(k, _)| *k),
            Some(format!("session-one:stall:{}", AFTER * 4).as_str())
        );
    }

    /// An owed decision that turns out to change nothing still ends the
    /// hold: an arming that failed, then `off`, leaves nothing due.
    #[test]
    fn an_owed_stall_that_needs_nothing_leaves_nothing_due() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(DONE, "t1")]);
        let blocked = stall_path(dir.path()).with_extension("json.tmp");
        std::fs::create_dir(&blocked).unwrap();
        let mut d = driver();
        d.reconcile(vec![stalling(dir.path(), &rollout)], 0);
        let off = Facts {
            stall_after: None,
            ..stalling(dir.path(), &rollout)
        };
        d.reconcile(vec![off], 1);
        d.due(RETRY_LOOK, NOW_GO);
        assert_eq!(d.next_due(), None);
    }

    fn stopped_agent() -> (tempfile::TempDir, PathBuf, Driver) {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &stop("t1")]);
        let mut d = driver();
        d.reconcile(vec![stalling(dir.path(), &rollout)], 0);
        d.due(AFTER, NOW_GO);
        assert_eq!(load_stall(dir.path()).unwrap().state, StallState::Quiet);
        (dir, rollout, d)
    }

    /// A quiet stall's record moves while storage is held: the re-arm is
    /// kept, and saved, counted from the movement, when the hold ends.
    #[test]
    fn a_rearm_observed_during_a_hold_is_saved_after_it() {
        let (dir, rollout, mut d) = stopped_agent();
        let blocked = stall_path(dir.path()).with_extension("json.tmp");
        std::fs::create_dir(&blocked).unwrap();
        append(&rollout, &[&line(STARTED, "t2")]);
        touched(&mut d, &rollout, AFTER + 1);
        assert_eq!(load_stall(dir.path()).unwrap().state, StallState::Quiet);
        std::fs::remove_dir(&blocked).unwrap();
        assert!(d.due(AFTER + 1 + RETRY_LOOK, NOW_GO).is_empty());
        let s = load_stall(dir.path()).unwrap();
        assert_eq!((s.state, s.since), (StallState::Waiting, AFTER + 1));
        assert_eq!(d.next_due(), Some(AFTER + 1 + AFTER));
        assert_eq!(notices(&d.due(AFTER + 1 + AFTER, NOW_GO)).len(), 1);
    }

    /// A newer decision during the hold supersedes the pending re-arm: a
    /// deliverer arriving leaves nothing stored and nothing due.
    #[test]
    fn a_newer_decision_supersedes_one_pending_in_a_hold() {
        let (dir, rollout, mut d) = stopped_agent();
        let blocked = stall_path(dir.path()).with_extension("json.tmp");
        std::fs::create_dir(&blocked).unwrap();
        append(&rollout, &[&line(STARTED, "t2")]);
        touched(&mut d, &rollout, AFTER + 1);
        let delivering = Facts {
            delivering: true,
            ..stalling(dir.path(), &rollout)
        };
        d.reconcile(vec![delivering], AFTER + 2);
        std::fs::remove_dir(&blocked).unwrap();
        assert!(d.due(AFTER + 1 + RETRY_LOOK, NOW_GO).is_empty());
        assert_eq!((load_stall(dir.path()), d.next_due()), (None, None));
    }

    /// Told, then not eligible and eligible again, all while storage is
    /// held: the latch is let go and a fresh interval counted from when it
    /// became eligible, with no notice until that interval passes.
    #[test]
    fn an_eligibility_reset_during_a_hold_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, rollout, mut d) = quiet_agent();
        assert_eq!(notices(&d.due(AFTER, NOW_GO)).len(), 1);
        let attending = |on: bool| Facts {
            attending: on,
            ..stalling(dir.path(), &rollout)
        };
        let perms = |mode| {
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap()
        };
        perms(0o555);
        d.reconcile(vec![attending(true)], 2500);
        d.reconcile(vec![attending(false)], 2501);
        perms(0o755);
        assert!(
            load_stall(dir.path()).unwrap().told(),
            "nothing written yet"
        );
        assert!(d.due(2500 + RETRY_LOOK, NOW_GO).is_empty());
        let s = load_stall(dir.path()).unwrap();
        assert_eq!((s.state, s.since), (StallState::Waiting, 2501));
        assert_eq!(waiting_for_you(dir.path()), None);
        let again = d.due(2501 + AFTER, NOW_GO);
        assert_eq!(
            notices_with_keys(&again).first().map(|(k, _)| *k),
            Some("session-one:stall:2501")
        );
    }

    fn block(dir: &Path) -> PathBuf {
        let blocked = stall_path(dir).with_extension("json.tmp");
        std::fs::create_dir(&blocked).unwrap();
        blocked
    }

    /// A notice that could not be stored is only a candidate: progress
    /// seen before it could be stored overtakes it, and nobody is told.
    #[test]
    fn an_unstored_notice_overtaken_by_progress_is_never_told() {
        let (dir, rollout, mut d) = quiet_agent();
        let blocked = block(dir.path());
        assert!(d.due(AFTER, NOW_GO).is_empty());
        append(&rollout, &[&line(STARTED, "t2")]);
        assert!(notices(&touched(&mut d, &rollout, AFTER + 1)).is_empty());
        std::fs::remove_dir(&blocked).unwrap();
        let acts = d.due(AFTER + RETRY_LOOK, NOW_GO);
        assert_eq!(notices_with_keys(&acts), [], "{acts:?}");
        let s = load_stall(dir.path()).unwrap();
        assert_eq!((s.state, s.since), (StallState::Waiting, AFTER + 1));
    }

    /// An unstored notice is decided again at the hold's end by the
    /// deadline's own gate: a deliverer, a new binding or auto mode off
    /// cancels it before any fold.
    #[test]
    fn an_unstored_notice_is_cancelled_by_the_gate_at_the_holds_end() {
        for gone in [Gate::Deliverer, Gate::SessionChanged, Gate::AutoOff] {
            let (dir, _, mut d) = quiet_agent();
            let blocked = block(dir.path());
            assert!(d.due(AFTER, NOW_GO).is_empty());
            std::fs::remove_dir(&blocked).unwrap();
            assert!(
                d.due(AFTER + RETRY_LOOK, |_, _| gone).is_empty(),
                "{gone:?}"
            );
            assert_eq!(load_stall(dir.path()), None, "{gone:?}");
        }
    }

    /// A later interval's notice is told though the stored one before it
    /// was told too: the latch is per interval.
    #[test]
    fn each_interval_is_told_once() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, rollout, mut d) = quiet_agent();
        assert_eq!(notices(&d.due(AFTER, NOW_GO)).len(), 1);
        let attending = |on: bool| Facts {
            attending: on,
            ..stalling(dir.path(), &rollout)
        };
        let perms = |mode| {
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap()
        };
        perms(0o555);
        d.reconcile(vec![attending(true)], 2500);
        d.reconcile(vec![attending(false)], 2501);
        assert!(d.due(2501 + AFTER, NOW_GO).is_empty(), "held");
        perms(0o755);
        let acts = d.due(2501 + AFTER + RETRY_LOOK, NOW_GO);
        assert_eq!(
            notices_with_keys(&acts)
                .iter()
                .map(|(k, _)| *k)
                .collect::<Vec<_>>(),
            ["session-one:stall:2501"]
        );
        let s = load_stall(dir.path()).unwrap();
        assert!(s.told() && s.since == 2501);
    }

    /// A fold that reaches an unstored notice before its deadline path,
    /// with progress in the record whose event never came, stores the
    /// progress, not the notice.
    #[test]
    fn a_fold_judges_an_unstored_notice_by_the_record() {
        let (dir, rollout, mut d) = quiet_agent();
        let blocked = block(dir.path());
        assert!(d.due(AFTER, NOW_GO).is_empty());
        append(&rollout, &[&line(STARTED, "t2")]);
        std::fs::remove_dir(&blocked).unwrap();
        let acts = d.reconcile(vec![stalling(dir.path(), &rollout)], AFTER + RETRY_LOOK);
        assert_eq!(notices_with_keys(&acts), [], "{acts:?}");
        let s = load_stall(dir.path()).unwrap();
        assert_eq!(
            (s.state, s.since),
            (StallState::Waiting, AFTER + RETRY_LOOK)
        );
        assert_eq!(d.next_due(), Some(AFTER + RETRY_LOOK + AFTER));
    }

    /// An interval lengthened while its notice was held is not told before
    /// its new deadline, and is told, once, at it.
    #[test]
    fn a_lengthened_interval_is_not_told_early() {
        let (dir, rollout, mut d) = quiet_agent();
        let blocked = block(dir.path());
        assert!(d.due(AFTER, NOW_GO).is_empty());
        let longer = AFTER + 1200;
        let f = || Facts {
            stall_after: Some(longer),
            ..stalling(dir.path(), &rollout)
        };
        d.reconcile(vec![f()], AFTER + 1);
        std::fs::remove_dir(&blocked).unwrap();
        assert!(notices(&d.due(AFTER + RETRY_LOOK, NOW_GO)).is_empty());
        let s = load_stall(dir.path()).unwrap();
        assert_eq!(
            (s.state, s.since, s.after),
            (StallState::Waiting, 0, longer)
        );
        assert_eq!(d.next_due(), Some(longer));
        let acts = d.due(longer, NOW_GO);
        assert!(
            matches!(notices(&acts).as_slice(), [t] if t.contains("for 1h")),
            "{acts:?}"
        );
    }
}
