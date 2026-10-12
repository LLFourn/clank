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

    /// Whether [`Self::latest`] is the record's latest turn: not when the
    /// record was entered mid-turn.
    pub(crate) fn anchored(&self) -> bool {
        !self.unanchored
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
    /// The current turn has ended: Claude closes a failed turn with the
    /// same marker as a finished one, and the first end is the true one.
    ended: bool,
}

impl RecordReader {
    pub(crate) fn new(tool: Tool, session: &str) -> Self {
        Self {
            tool,
            session: session.to_string(),
            current: None,
            ended: false,
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
        match &fact {
            Fact::Started { id, .. } => {
                self.current = Some(id.clone());
                self.ended = false;
            }
            Fact::Ended { id: None, .. } if self.ended && self.tool == Tool::Claude => {
                return None;
            }
            Fact::Ended { .. } => self.ended = true,
            Fact::NativeRetry => self.ended = false,
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
/// changed; a tool result shares its turn's id. A failure, a person's
/// interrupt and the `turn_duration` written as any turn closes name no
/// turn.
fn claude(v: &serde_json::Value, current: Option<&str>) -> Option<Fact> {
    match v.get("type")?.as_str()? {
        "system" if string(v, "subtype").as_deref() == Some("turn_duration") => Some(Fact::Ended {
            id: None,
            outcome: Outcome::Completed,
        }),
        "user" => {
            let id = string(v, "promptId")?;
            if Some(id.as_str()) == current {
                let interrupted = v
                    .get("message")?
                    .get("content")?
                    .as_array()?
                    .iter()
                    .filter_map(|b| b.get("text")?.as_str())
                    .any(|t| t.starts_with("[Request interrupted by user"));
                return interrupted.then_some(Fact::Ended {
                    id: None,
                    outcome: Outcome::Interrupted,
                });
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
    /// The wake typed into the agent's pane and submitted, with no
    /// interrupt, after a last look at its input line.
    Typed,
}

impl Transport {
    fn name(self) -> &'static str {
        match self {
            Transport::CodexStart => "codex-start",
            Transport::Helper => "helper",
            Transport::Typed => "typed",
        }
    }
}

/// The transport clank may use for `tool`, or why it may not: the harness's
/// resume capability ([`crate::cli::resume`]), which committed evidence for
/// its installed version must enable. Recovery never cancels a turn, so a
/// harness with none waits for a person.
pub(crate) fn transport_for(tool: Tool) -> Result<Transport, &'static str> {
    #[cfg(test)]
    if let Some(t) = tests::enabled(tool) {
        return t.ok_or(crate::cli::resume::NO_EVIDENCE);
    }
    match crate::cli::resume::resume_for(tool)? {
        crate::cli::resume::Resume::TypedSubmit => Ok(Transport::Typed),
        crate::cli::resume::Resume::Plugin => Ok(Transport::Helper),
    }
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
    #[serde(default)]
    pub(crate) kind: Kind,
    /// A fallback's interval, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) every: Option<u64>,
    /// The roster role it was opened under: its attempts are that role's,
    /// and another role's fold or restart ends it.
    #[serde(default)]
    pub(crate) role: String,
}

/// What opened an episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    /// A turn that ended on a provider error.
    #[default]
    Failed,
    /// A turn that ended the ordinary way with work still owed, and no
    /// wait to take it.
    Idle,
    /// `recovery.blind_nudge`: work owed, and a record that cannot be read.
    Fallback,
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
}

/// `recovery.blind_nudge` for an agent whose record cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fallback {
    /// The stuck interval: the agent's owed work, by its items' stable
    /// identities. Different work is a different interval.
    pub(crate) identity: String,
    pub(crate) every: u64,
}

/// What a decision needs besides the episode and the snapshot.
pub(crate) struct Ctx<'a> {
    pub(crate) now: u64,
    pub(crate) session: &'a str,
    pub(crate) tool: Tool,
    pub(crate) transport: Option<Transport>,
    /// Why `transport` is `None`, for the agent's status.
    pub(crate) no_transport: &'static str,
    /// What the monitor found when it checked, where it checked.
    pub(crate) gate: Option<Gate>,
    /// The agent owes work and has no wait to take it.
    pub(crate) obligated: bool,
    /// Its record is being read.
    pub(crate) readable: bool,
    pub(crate) fallback: Option<Fallback>,
    /// The agent's roster role.
    pub(crate) role: &'a str,
    /// The highest attempt number ever issued to this agent, in any
    /// episode or interval: a claim continues past it, so no two attempts,
    /// their answers, tags or history share an identity.
    pub(crate) issued: u32,
}

/// How long a claimed attempt has for its transport to answer.
pub(crate) const ANSWER_WITHIN: u64 = 60;
/// How long a submitted attempt has to show up in the record.
pub(crate) const CONFIRM_WITHIN: u64 = 120;
/// How often a held or transportless episode is looked at again, and how
/// many absent transports it tolerates.
const RETRY_LOOK: u64 = 60;
const UNAVAILABLE_LIMIT: usize = 3;
/// The waits before an idle agent's nudges: a minute for its state to
/// settle and two more, then fifteen after a nudged turn that took nothing
/// up.
const IDLE_WAITS: &[u64] = &[180, 900];
pub(crate) const IDLE: &str = "idle";
pub(crate) const FALLBACK: &str = "fallback";
/// A blind nudge's checks that found nothing to send to, and the longest
/// it waits between them.
const FALLBACK_CHECKS: usize = 5;
const FALLBACK_CEILING: u64 = 4 * 3600;

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
    // An episode is its session's and its role's: another role owes other
    // work, so what was armed for the old one is over, however late the
    // change is seen.
    let mut ep = prev
        .filter(|e| e.session == ctx.session && e.role == ctx.role)
        .filter(|e| e.kind != Kind::Fallback || fallback_stands(e, ctx));

    if let Some(e) = ep.as_mut().filter(|e| e.live() && e.kind != Kind::Fallback) {
        follow(e, snap, ctx);
    }
    let free = |ep: &Option<Episode>, id: &str| {
        ep.as_ref()
            .is_none_or(|e| !e.live() && e.kind != Kind::Fallback && !e.owns(id))
    };
    if let Some(latest) = snap.latest().filter(|t| !t.retrying)
        && free(&ep, &latest.id)
    {
        match &latest.outcome {
            Outcome::Failed(class) => {
                ep = Some(opened(Kind::Failed, latest.id.clone(), class.clone(), ctx))
            }
            Outcome::Completed if ctx.obligated && snap.anchored() => {
                ep = Some(opened(Kind::Idle, latest.id.clone(), IDLE.into(), ctx))
            }
            _ => {}
        }
    }
    if ep
        .as_ref()
        .is_none_or(|e| !e.live() && e.kind != Kind::Fallback)
        && ctx.obligated
        && !ctx.readable
        && let Some(f) = &ctx.fallback
    {
        ep = Some(Episode {
            kind: Kind::Fallback,
            every: Some(f.every),
            next_at: Some(ctx.now + f.every),
            ..blank(f.identity.clone(), String::new(), FALLBACK.into(), ctx)
        });
    }

    let mut effects = Vec::new();
    if let Some(e) = ep.as_mut().filter(|e| e.live()) {
        match (ctx.gate, e.state, e.next_at) {
            // Held: no deadline, so nothing looks again until a fold says
            // the hold is over.
            (Some(Gate::Hold | Gate::AutoOff), State::Waiting, _) => e.next_at = None,
            (_, State::Waiting, None) => e.next_at = Some(ctx.now + RETRY_LOOK),
            _ => effects.extend(due(e, snap, ctx)),
        }
    }
    (ep, effects)
}

/// Whether a fallback episode still describes the agent: its record still
/// unreadable and its work the same. Otherwise it is over, and forgotten.
fn fallback_stands(e: &Episode, ctx: &Ctx) -> bool {
    !ctx.readable
        && ctx
            .fallback
            .as_ref()
            .is_some_and(|f| f.identity == e.opened_by)
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

    /// The wait before the next attempt, or why there is none. A failure
    /// keeps its class's budget; an idle episode has its own, and a nudged
    /// turn that fails spends from both, so switching refunds nothing.
    fn next_wait(&self) -> Result<u64, &'static str> {
        let spent = self.spent();
        let budget = match self.class.as_str() {
            IDLE => IDLE_WAITS,
            class => retries(class),
        };
        if budget.is_empty() {
            return Err("this kind of failure needs a person");
        }
        let idle_left = self.kind != Kind::Idle || spent < IDLE_WAITS.len();
        match budget.get(spent) {
            Some(wait) if idle_left => Ok(*wait),
            _ if self.kind == Kind::Idle => Err("nudging did not get it going"),
            _ => Err("retrying did not recover it"),
        }
    }

    /// The outcome a turn must have to be what this episode is about.
    fn targets(&self, outcome: &Outcome) -> bool {
        match self.class.as_str() {
            IDLE => *outcome == Outcome::Completed,
            _ => matches!(outcome, Outcome::Failed(_)),
        }
    }
}

fn blank(opened_by: String, turn: String, class: String, ctx: &Ctx) -> Episode {
    Episode {
        session: ctx.session.to_string(),
        opened_by,
        failed_turn: turn,
        class,
        attempts: Vec::new(),
        next_at: None,
        state: State::Waiting,
        notice: None,
        unobserved: 0,
        kind: Kind::Failed,
        every: None,
        role: ctx.role.to_string(),
    }
}

fn opened(kind: Kind, id: String, class: String, ctx: &Ctx) -> Episode {
    let mut e = Episode {
        kind,
        ..blank(id.clone(), id, class, ctx)
    };
    // An idle agent is given its settle period first, transport or not: a
    // turn's end is read moments before the Stop hook parks it.
    if kind == Kind::Failed && ctx.transport.is_none() {
        escalate(&mut e, ctx, ctx.no_transport);
        return e;
    }
    match e.next_wait() {
        Err(why) => escalate(&mut e, ctx, why),
        Ok(wait) => e.next_at = Some(ctx.now + wait),
    }
    e
}

fn escalate(e: &mut Episode, ctx: &Ctx, why: &str) {
    let tool = ctx.tool.as_str();
    let what = match e.kind {
        Kind::Failed => format!("{tool}'s turn failed ({})", e.class),
        Kind::Idle if e.class == IDLE => format!("{tool} is idle with pending work"),
        Kind::Idle => format!("{tool}'s nudged turn failed ({})", e.class),
        Kind::Fallback => format!("{tool} has pending work and its record cannot be read"),
    };
    e.notice = Some(format!(
        "{what} and {why}; its pending work is waiting for you"
    ));
    e.state = State::Escalated;
    e.next_at = None;
}

/// Bring a live episode up to what the record now says.
fn follow(e: &mut Episode, snap: &Snapshot, ctx: &Ctx) {
    if snap
        .find(&e.failed_turn)
        .is_some_and(|t| !e.targets(&t.outcome) && t.outcome != Outcome::Running)
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
        // Already this episode's target: decided when it ended.
        .filter(|t| t.id != e.failed_turn)
    else {
        return;
    };
    let next = match (&turn.outcome, e.state) {
        (Outcome::Running, _) => return,
        // A nudged turn that ended and left the work owed, with no wait to
        // take it: still idle, on the same budget.
        (Outcome::Completed, State::Running) if e.kind == Kind::Idle => IDLE.to_string(),
        (Outcome::Completed | Outcome::Interrupted, _) => return close(e),
        (Outcome::Failed(class), State::Running) => class.clone(),
        // Already decided: waiting for the next attempt, or latched for a
        // person, whom a late failure of the uncertain attempt changes
        // nothing for.
        (Outcome::Failed(_), _) => return,
    };
    e.failed_turn = turn.id.clone();
    e.class = next;
    match e.next_wait() {
        _ if ctx.transport.is_none() => escalate(e, ctx, ctx.no_transport),
        Ok(wait) => {
            e.state = State::Waiting;
            e.next_at = Some(ctx.now + wait);
        }
        Err(why) => escalate(e, ctx, why),
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
            // Delimited, so attempt 1's tag is not read in attempt 12's.
            || turn
                .text
                .as_deref()
                .is_some_and(|t| t.contains(&format!("({})", self.tag))))
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
        State::Waiting if e.kind == Kind::Fallback => {
            // Its record is unreadable by definition, so that is no reason
            // to wait; anything else the gate says is.
            if !matches!(ctx.gate, Some(Gate::Go | Gate::Unobserved)) {
                e.next_at = Some(ctx.now + fallback_wait(e));
                return None;
            }
            let Some(transport) = ctx.transport else {
                escalate(e, ctx, ctx.no_transport);
                return None;
            };
            Some(claim(e, transport, ctx))
        }
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
            let current = snap.anchored()
                && snap
                    .latest()
                    .is_some_and(|t| t.id == e.failed_turn && e.targets(&t.outcome) && !t.retrying);
            if !current {
                e.next_at = Some(ctx.now + RETRY_LOOK);
                return None;
            }
            Some(claim(e, transport, ctx))
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

/// Claim the next attempt, durably before anything is sent.
fn claim(e: &mut Episode, transport: Transport, ctx: &Ctx) -> Effect {
    let n = e
        .attempts
        .iter()
        .map(|a| a.n)
        .chain([ctx.issued])
        .max()
        .unwrap_or(0)
        + 1;
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
    Effect::Dispatch {
        n,
        tag,
        target: e.failed_turn.clone(),
    }
}

/// A blind nudge's wait before its next check: its interval, doubled for
/// each check that found nothing to send to, up to four hours.
fn fallback_wait(e: &Episode) -> u64 {
    e.every
        .unwrap_or(RETRY_LOOK)
        .saturating_mul(1 << e.unavailable().min(16))
        .min(FALLBACK_CEILING)
}

/// A transport's answer, applied only to the attempt it was given.
pub(crate) fn answered(mut e: Episode, n: u32, answer: Answer, ctx: &Ctx) -> Episode {
    let current = e.state == State::Dispatched
        && e.attempts
            .last()
            .is_some_and(|a| a.n == n && a.disposition == Disposition::Claimed);
    if !current {
        return e;
    }
    let a = e.attempts.last_mut().expect("checked");
    match answer {
        Answer::Submitted { started } if e.kind == Kind::Fallback => {
            // With its record unreadable nothing can confirm it, so it is
            // never sent again in this interval.
            a.disposition = Disposition::Submitted;
            a.started = started;
            escalate(
                &mut e,
                ctx,
                "a blind nudge was sent that clank cannot see the outcome of",
            );
        }
        Answer::Submitted { started } => {
            a.disposition = Disposition::Submitted;
            a.started = started;
            e.next_at = Some(a.claimed_at + CONFIRM_WITHIN);
        }
        Answer::Refused => {
            a.disposition = Disposition::Refused;
            close(&mut e);
        }
        Answer::Unavailable if e.kind == Kind::Fallback => {
            a.disposition = Disposition::Unavailable;
            if e.unavailable() >= FALLBACK_CHECKS {
                escalate(
                    &mut e,
                    ctx,
                    "a blind nudge never found it ready to take one",
                );
            } else {
                e.state = State::Waiting;
                e.next_at = Some(ctx.now + fallback_wait(&e));
            }
        }
        Answer::Unavailable => {
            a.disposition = Disposition::Unavailable;
            if e.unavailable() >= UNAVAILABLE_LIMIT {
                escalate(&mut e, ctx, "its recovery transport never answered");
            } else {
                e.state = State::Waiting;
                e.next_at = Some(ctx.now + RETRY_LOOK);
            }
        }
        Answer::Indeterminate => {
            a.disposition = Disposition::Indeterminate;
            escalate(&mut e, ctx, "a retry may or may not have reached it");
        }
    }
    e
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
    /// Its roster role.
    pub(crate) role: String,
    /// The stable identities of the work it owes.
    pub(crate) work: Vec<String>,
    /// `recovery.blind_nudge`, in seconds; `None` is `off`.
    pub(crate) blind_nudge: Option<u64>,
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
        /// What it submits.
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
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
    role: String,
    attending: bool,
    asking: bool,
    delivering: bool,
    fallback: Option<Fallback>,
    /// The activity last published, and since when.
    activity: Option<(crate::cli::agent_state::Activity, u64)>,
    published: Option<crate::cli::agent_state::AgentState>,
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
    /// Each dispatched attempt not yet settled in its agent's nudge
    /// history, and where that agent keeps its pending file. Captured at
    /// dispatch and saved beside the agent, so it outlives the episode, the
    /// agent's place on the roster, and the monitor itself.
    pending:
        std::collections::BTreeMap<(clank_core::ids::AgentLabel, AttemptRef), (PathBuf, Pending)>,
    /// This monitor, as each published state names it.
    observer: crate::cli::agent_state::Observer,
}

/// What an attempt's history record needs, as it was when dispatched, and
/// when it is settled as unanswered if nothing answers first.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Pending {
    pub(crate) attempt: AttemptRef,
    pub(crate) at: u64,
    pub(crate) class: String,
    #[serde(default)]
    pub(crate) kind: Kind,
    /// Its place in its own episode's budget: one for the first attempt
    /// after any reset, whatever its number.
    #[serde(default)]
    pub(crate) ordinal: u32,
    /// [`Transport::name`].
    pub(crate) transport: String,
    pub(crate) text: String,
    pub(crate) due: u64,
}

impl Pending {
    fn record(
        &self,
        at: u64,
        outcome: crate::cli::nudge_log::Outcome,
    ) -> crate::cli::nudge_log::Record {
        crate::cli::nudge_log::Record {
            at,
            attempt: format!(
                "recovery:{}:{}:{}",
                self.attempt.session, self.attempt.opened_by, self.attempt.n
            ),
            trigger: match (self.kind, self.class.as_str()) {
                (Kind::Fallback, _) => crate::cli::nudge_log::Trigger::Fallback {
                    attempt: self.place(),
                },
                (_, IDLE) => crate::cli::nudge_log::Trigger::Idle {
                    attempt: self.place(),
                    of: IDLE_WAITS.len() as u32,
                },
                (_, class) => crate::cli::nudge_log::Trigger::Retry {
                    class: class.to_string(),
                    attempt: self.place(),
                    of: retries(class).len() as u32,
                },
            },
            text: self.text.clone(),
            outcome,
        }
    }

    /// Its place in its budget, for a pending attempt saved before places
    /// were: its number, which was its place until a reset.
    fn place(&self) -> u32 {
        match self.ordinal {
            0 => self.attempt.n,
            n => n,
        }
    }

    /// What `answer` lets the history claim. A helper's `Submitted` is its
    /// report before the exit that wakes the harness; only Codex's
    /// `turn/start` answers for the transport itself.
    fn outcome(&self, answer: &Answer) -> crate::cli::nudge_log::Outcome {
        use crate::cli::nudge_log::Outcome;
        match answer {
            Answer::Submitted { .. } if self.transport == Transport::Helper.name() => {
                Outcome::Reported
            }
            Answer::Submitted { .. } => Outcome::Submitted,
            Answer::Refused => Outcome::Refused {
                reason: "the agent was no longer idle at the failed turn".into(),
            },
            Answer::Unavailable => Outcome::Refused {
                reason: format!("no {} was there to try", self.transport),
            },
            Answer::Indeterminate => Outcome::Indeterminate {
                reason: "it may have reached the harness".into(),
            },
        }
    }
}

fn pending_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("nudge-pending.json")
}

fn load_pending(agent_dir: &Path) -> Vec<Pending> {
    std::fs::read(pending_path(agent_dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

impl Driver {
    pub(crate) fn new(cwd: PathBuf, home: PathBuf) -> Self {
        Self {
            agents: Default::default(),
            installed: Default::default(),
            lost: Default::default(),
            cwd,
            home,
            pending: Default::default(),
            observer: crate::cli::agent_state::Observer::me(),
        }
    }

    /// Remember each dispatch in `acts` as pending history.
    fn accounted(&mut self, acts: Vec<Act>, now: u64) -> Vec<Act> {
        for act in &acts {
            let Act::Dispatch {
                author,
                attempt,
                transport,
                text,
                ..
            } = act
            else {
                continue;
            };
            let Some(w) = self.agents.get(author) else {
                continue;
            };
            let (class, kind, ordinal) = w
                .episode
                .as_ref()
                .filter(|e| e.opened_by == attempt.opened_by)
                .map(|e| {
                    // A fallback sends at most once; its place is its check.
                    let place = match e.kind {
                        Kind::Fallback => e.attempts.len(),
                        _ => e.spent(),
                    };
                    (e.class.clone(), e.kind, place as u32)
                })
                .unwrap_or_default();
            let pending = Pending {
                attempt: attempt.clone(),
                at: now,
                class,
                kind,
                ordinal,
                transport: transport.name().into(),
                text: text.clone(),
                due: now + ANSWER_WITHIN,
            };
            self.pending.insert(
                (author.clone(), attempt.clone()),
                (w.agent_dir.clone(), pending),
            );
            self.save_pending(author);
        }
        acts
    }

    /// Settle `attempt`'s history with `outcome`, once: whatever the episode
    /// has since become, and whether or not recovery still acts on it. The
    /// row is appended before the attempt stops being pending, so a stop
    /// between the two leaves it to settle again, and the journal keeps
    /// one row per attempt. A failed append loses the row, never a retry.
    fn settle(
        &mut self,
        author: &clank_core::ids::AgentLabel,
        attempt: &AttemptRef,
        outcome: impl FnOnce(&Pending) -> crate::cli::nudge_log::Outcome,
        now: u64,
    ) {
        self.settle_with(author, attempt, outcome, now, |dir, record| {
            if let Err(e) = crate::cli::nudge_log::append(dir, record) {
                eprintln!("monitor: could not record `{author}`'s nudge: {e}");
            }
        });
    }

    /// [`Self::settle`] with the append supplied.
    fn settle_with(
        &mut self,
        author: &clank_core::ids::AgentLabel,
        attempt: &AttemptRef,
        outcome: impl FnOnce(&Pending) -> crate::cli::nudge_log::Outcome,
        now: u64,
        append: impl FnOnce(&Path, &crate::cli::nudge_log::Record),
    ) {
        let key = (author.clone(), attempt.clone());
        let Some((dir, pending)) = self.pending.get(&key) else {
            return;
        };
        append(dir, &pending.record(now, outcome(pending)));
        let (dir, _) = self.pending.remove(&key).expect("present");
        self.save_pending_in(author, &dir);
    }

    /// Every attempt saved as pending in the repo's agent directories,
    /// found once when the monitor starts: an attempt outlives its agent's
    /// place on the roster as well as the monitor that sent it.
    pub(crate) fn discover_pending(&mut self, agents_root: &Path) {
        let Ok(entries) = std::fs::read_dir(agents_root) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(author) =
                clank_core::ids::AgentLabel::parse(&entry.file_name().to_string_lossy())
            else {
                continue;
            };
            let dir = entry.path();
            for p in load_pending(&dir) {
                self.pending
                    .entry((author.clone(), p.attempt.clone()))
                    .or_insert((dir.clone(), p));
            }
        }
    }

    fn save_pending(&self, author: &clank_core::ids::AgentLabel) {
        if let Some(w) = self.agents.get(author) {
            self.save_pending_in(author, &w.agent_dir);
        }
    }

    /// Write `author`'s pending attempts into `dir`, or remove the file when
    /// none is left. Best effort: a lost file loses history, never a retry.
    fn save_pending_in(&self, author: &clank_core::ids::AgentLabel, dir: &Path) {
        let mine: Vec<&Pending> = self
            .pending
            .iter()
            .filter(|((a, _), _)| a == author)
            .map(|(_, (_, p))| p)
            .collect();
        let path = pending_path(dir);
        if mine.is_empty() {
            let _ = std::fs::remove_file(path);
            return;
        }
        if let Ok(bytes) = serde_json::to_vec(&mine) {
            let tmp = path.with_extension("json.tmp");
            let _ = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, &path));
        }
    }

    /// Settle every pending attempt whose answer never came by its deadline,
    /// whatever has become of its agent's recovery or roster place.
    fn settle_expired(&mut self, now: u64) {
        let expired: Vec<(clank_core::ids::AgentLabel, AttemptRef)> = self
            .pending
            .iter()
            .filter(|(_, (_, p))| p.due <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for (author, attempt) in expired {
            self.settle(
                &author,
                &attempt,
                |_| crate::cli::nudge_log::Outcome::Indeterminate {
                    reason: "no answer came".into(),
                },
                now,
            );
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
                episode
            })
            .chain(self.pending.values().map(|(_, p)| p.due))
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
            if !self.agents.contains_key(&f.author) {
                for p in load_pending(&f.agent_dir) {
                    self.pending
                        .entry((f.author.clone(), p.attempt.clone()))
                        .or_insert((f.agent_dir.clone(), p));
                }
            }
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
                    role: String::new(),
                    attending: false,
                    asking: false,
                    delivering: false,
                    fallback: None,
                    activity: None,
                    published: crate::cli::agent_state::load(&f.agent_dir),
                });
            w.auto = f.auto;
            w.obligated = f.obligated && !f.delivering;
            w.role = f.role.clone();
            w.attending = f.attending;
            w.asking = f.asking;
            w.delivering = f.delivering;
            w.fallback = f.blind_nudge.map(|every| Fallback {
                identity: fallback_identity(&f.work),
                every,
            });
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
                w.activity = None;
            }
            if !w.obligated {
                let gate = if f.delivering {
                    Gate::Deliverer
                } else {
                    Gate::ObligationCleared
                };
                acts.extend(decision(&f.author, w, now, Some(gate), None));
                if f.asking {
                    watch_record(w, &f.session, &self.cwd, &self.home);
                } else {
                    disarm(w);
                }
                continue;
            }
            watch_record(w, &f.session, &self.cwd, &self.home);
            // Decided on every fold: only a fold says whether the agent is
            // still eligible, so only a fold can take away, or give back,
            // its permission to be sent anything.
            acts.extend(decision(&f.author, w, now, suppressed(w), None));
        }
        self.lost.clear();
        acts.extend(self.sync_watches());
        self.publish(now);
        self.accounted(acts, now)
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
                    let was = w.read.is_some();
                    let armed = ensure(w);
                    let lost = !armed && !refresh(w) && was && w.read.is_none();
                    if (armed || lost || w.read.is_some()) && w.obligated {
                        acts.extend(decision(author, w, now, suppressed(w), None));
                    }
                }
            }
        }
        acts.extend(self.sync_watches());
        self.publish(now);
        self.accounted(acts, now)
    }

    /// Fire every deadline that has passed, after reading the record and
    /// asking `gate` about the agent as it is now.
    pub(crate) fn due(
        &mut self,
        now: u64,
        gate: impl Fn(&clank_core::ids::AgentLabel, &str) -> Gate,
    ) -> Vec<Act> {
        // An answer already on disk is the attempt's evidence: read it
        // before concluding that none came.
        let mut acts = self.replies(now);
        self.settle_expired(now);
        for (author, w) in self.agents.iter_mut() {
            let due = w
                .episode
                .as_ref()
                .filter(|e| e.live())
                .and_then(|e| e.next_at)
                .is_some_and(|at| at <= now)
                || w.owed;
            let due = due && w.held_until.is_none_or(|at| at <= now);
            if !due {
                continue;
            }
            let fresh = refresh(w);
            let mut g = gate(author, &w.session);
            if g == Gate::AutoOff {
                w.auto = false;
            }
            if g == Gate::Go && !fresh {
                g = Gate::Unobserved;
            }
            // A decision owed across a hold is made from the agent as the
            // folds since have left it, not only as the gate sees it now.
            if !w.obligated {
                g = match w.delivering {
                    true => Gate::Deliverer,
                    false => Gate::ObligationCleared,
                };
            } else if let (Gate::Go | Gate::Unobserved, Some(held)) = (g, suppressed(w)) {
                g = held;
            }
            acts.extend(decision(author, w, now, Some(g), None));
        }
        self.publish(now);
        self.accounted(acts, now)
    }

    /// A transport's answer, applied only to the attempt it was for.
    pub(crate) fn answer(
        &mut self,
        author: &clank_core::ids::AgentLabel,
        attempt: &AttemptRef,
        answer: Answer,
        now: u64,
    ) -> Vec<Act> {
        self.settle(author, attempt, |p| p.outcome(&answer), now);
        let mut acts = Vec::new();
        if let Some(w) = self.agents.get_mut(author) {
            acts.extend(decision(author, w, now, None, Some((attempt, answer))));
        }
        self.publish(now);
        self.accounted(acts, now)
    }

    /// Whether `attempt` may still be sent to `author` now: still this
    /// agent's claimed attempt, on its binding and unsuppressed; the role
    /// and work derived `live`, from disk at the send, still the ones it was
    /// claimed under; and, the record read afresh, its target still the
    /// latest, ended turn. A fallback's record must still be unreadable,
    /// and its live work still the interval's.
    pub(crate) fn may_send(
        &mut self,
        author: &clank_core::ids::AgentLabel,
        attempt: &AttemptRef,
        live: &Live,
    ) -> bool {
        let Some(w) = self.agents.get_mut(author) else {
            return false;
        };
        let Some(e) = w.episode.clone() else {
            return false;
        };
        let claimed = e.session == attempt.session
            && e.opened_by == attempt.opened_by
            && e.state == State::Dispatched
            && e.attempts
                .last()
                .is_some_and(|a| a.n == attempt.n && a.disposition == Disposition::Claimed);
        if !claimed
            || w.session != attempt.session
            || !w.obligated
            || suppressed(w).is_some()
            || live.role != e.role
            || w.role != e.role
            || live.work.is_empty()
        {
            return false;
        }
        if e.kind == Kind::Fallback {
            return w.read.is_none()
                && w.fallback.is_some()
                && fallback_identity(&live.work) == e.opened_by;
        }
        refresh(w)
            && w.snap.anchored()
            && w.snap
                .latest()
                .is_some_and(|t| t.id == e.failed_turn && e.targets(&t.outcome) && !t.retrying)
    }

    /// Publish each agent's state that changed in meaning.
    fn publish(&mut self, now: u64) {
        for w in self.agents.values_mut() {
            let activity = activity(w);
            let since = match &w.activity {
                Some((a, since)) if *a == activity => *since,
                _ => now,
            };
            w.activity = Some((activity.clone(), since));
            let state = crate::cli::agent_state::AgentState {
                role: w.role.clone(),
                session: w.session.clone(),
                tool: w.tool,
                monitor: self.observer.clone(),
                activity,
                since,
                suppressors: suppressors(w),
                nudge: nudge(w.episode.as_ref()),
                cannot_resume: transport_for(w.tool).err().map(str::to_string),
            };
            if w.published.as_ref() != Some(&state) {
                crate::cli::agent_state::store(&w.agent_dir, &state);
                w.published = Some(state);
            }
        }
    }

    /// Answers helpers left beside their agents, and beside any agent that
    /// still has an attempt pending though it is no longer on the roster.
    pub(crate) fn replies(&mut self, now: u64) -> Vec<Act> {
        let mut acts = Vec::new();
        let mut answered = Vec::new();
        let away: std::collections::BTreeMap<clank_core::ids::AgentLabel, PathBuf> = self
            .pending
            .iter()
            .filter(|((author, _), _)| !self.agents.contains_key(author))
            .map(|((author, _), (dir, _))| (author.clone(), dir.clone()))
            .collect();
        for (author, dir) in away {
            if let Some(reply) = take_reply(&dir) {
                answered.push((author, reply.attempt(), reply.answer()));
            }
        }
        for (author, w) in self.agents.iter_mut() {
            let Some(reply) = take_reply(&w.agent_dir) else {
                continue;
            };
            let attempt = reply.attempt();
            answered.push((author.clone(), attempt.clone(), reply.answer()));
            acts.extend(decision(
                author,
                w,
                now,
                None,
                Some((&attempt, reply.answer())),
            ));
        }
        for (author, attempt, answer) in &answered {
            self.settle(author, attempt, |p| p.outcome(answer), now);
        }
        self.publish(now);
        self.accounted(acts, now)
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
        // What was read no longer describes a record that can be read.
        w.read = None;
        w.snap = Snapshot::default();
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
    let (transport, no_transport) = match transport_for(w.tool) {
        Ok(t) => (Some(t), ""),
        Err(why) => (None, why),
    };
    let gate = match gate {
        None if !w.auto => Some(Gate::AutoOff),
        g => g,
    };
    let ctx = Ctx {
        now,
        session: &w.session,
        tool: w.tool,
        transport,
        no_transport,
        gate,
        obligated: w.obligated,
        readable: w.read.is_some(),
        fallback: w.fallback.clone(),
        role: &w.role,
        issued: 0,
    };
    w.answers
        .extend(answer.map(|(a, answer)| (a.clone(), answer)));
    if w.held_until.is_some_and(|at| at > now) {
        // Decided when the hold ends, from the state then.
        w.owed = true;
        return Vec::new();
    }
    // Read only past the hold, and after any answer is kept: a fault here
    // holds new claims, never the results of claims already made.
    let issued = match load_issued(&w.agent_dir) {
        Ok(n) => n,
        Err(e) => {
            eprintln!(
                "monitor: could not read `{author}`'s issued attempts, so nothing was done: {e}"
            );
            w.held_until = Some(now + RETRY_LOOK);
            w.owed = true;
            return Vec::new();
        }
    };
    let ctx = Ctx { issued, ..ctx };
    let prev = w.episode.clone();
    let mut ep = prev.clone();
    for (attempt, answer) in w.answers.clone() {
        ep = match ep {
            Some(e) if e.session == attempt.session && e.opened_by == attempt.opened_by => {
                Some(answered(e, attempt.n, answer, &ctx))
            }
            other => other,
        };
    }
    // An answer may name a turn the record has already shown.
    let (next, effects) = decide(ep, &w.snap, &ctx);
    let high = next
        .iter()
        .flat_map(|e| &e.attempts)
        .map(|a| a.n)
        .max()
        .unwrap_or(0);
    // The number is claimed before the episode that uses it, and neither
    // is acted on until both are stored: an attempt's identity must
    // outlive its episode, which a reset may forget.
    let stored = match high > issued {
        true => store_issued(&w.agent_dir, high),
        false => Ok(()),
    }
    .and_then(|()| match next != prev {
        true => store(&w.agent_dir, next.as_ref()),
        false => Ok(()),
    });
    if let Err(e) = stored {
        eprintln!("monitor: could not record `{author}`'s recovery, so nothing was done: {e}");
        w.held_until = Some(now + RETRY_LOOK);
        w.owed = true;
        return Vec::new();
    }
    w.held_until = None;
    w.owed = false;
    w.answers.clear();
    w.episode = next;
    let Some(e) = w.episode.as_ref() else {
        return Vec::new();
    };
    effects
        .into_iter()
        .filter_map(|Effect::Dispatch { n, tag, target }| {
            transport.map(|transport| Act::Dispatch {
                author: author.clone(),
                attempt: AttemptRef {
                    session: e.session.clone(),
                    opened_by: e.opened_by.clone(),
                    n,
                },
                transport,
                text: match e.kind {
                    Kind::Failed => wake_text(&tag),
                    Kind::Idle | Kind::Fallback => nudge_text(&tag),
                },
                tag,
                target,
            })
        })
        .collect()
}

/// What is true of an agent at a send, derived from disk then rather than
/// from the last fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Live {
    pub(crate) role: String,
    /// The stable identities of the work it owes.
    pub(crate) work: Vec<String>,
}

/// What holds the agent's nudges back just now, as the last fold saw it:
/// manual mode, or a question, block or attendance.
fn suppressed(w: &Watched) -> Option<Gate> {
    if !w.auto {
        Some(Gate::AutoOff)
    } else if w.attending || w.asking {
        Some(Gate::Hold)
    } else {
        None
    }
}

/// What the agent is doing, by what its record shows.
fn activity(w: &Watched) -> crate::cli::agent_state::Activity {
    use crate::cli::agent_state::Activity;
    if w.asking {
        return Activity::Asking;
    }
    if w.read.is_none() {
        if !w.obligated {
            return Activity::Unobserved;
        }
        return Activity::Unknown(
            match &w.source {
                Some((_, Some(Source::At(_)))) => "its record could not be read",
                Some((_, Some(_))) => "its record has not appeared yet",
                _ => "no record was found for its session",
            }
            .into(),
        );
    }
    match w.snap.latest().filter(|_| w.snap.anchored()) {
        None => Activity::Unknown("its record shows no turn yet".into()),
        Some(t) => match &t.outcome {
            Outcome::Running => Activity::Working,
            Outcome::Completed => Activity::Idle,
            Outcome::Failed(class) => Activity::Failed(class.clone()),
            Outcome::Interrupted => Activity::Interrupted,
        },
    }
}

fn suppressors(w: &Watched) -> Vec<crate::cli::agent_state::Suppressor> {
    use crate::cli::agent_state::Suppressor;
    [
        (w.attending, Suppressor::Attending),
        (w.delivering, Suppressor::Delivering),
        (
            w.snap.latest().is_some_and(|t| t.retrying),
            Suppressor::NativeRetry,
        ),
        (!w.auto, Suppressor::AutoOff),
    ]
    .into_iter()
    .filter_map(|(on, s)| on.then_some(s))
    .collect()
}

fn nudge(ep: Option<&Episode>) -> crate::cli::agent_state::Nudge {
    use crate::cli::agent_state::Nudge;
    match ep {
        Some(e) => match e.state {
            State::Waiting => e.next_at.map_or(Nudge::Nothing, Nudge::At),
            State::Dispatched | State::Running => Nudge::InFlight,
            State::Escalated => Nudge::WaitingForYou(e.notice.clone().unwrap_or_default()),
            State::Closed => Nudge::Nothing,
        },
        None => Nudge::Nothing,
    }
}

/// One stable name for `work`, whatever order it was listed in: FNV-1a,
/// so the same work names the same interval across builds and restarts.
fn fallback_identity(work: &[String]) -> String {
    let mut sorted: Vec<&String> = work.iter().collect();
    sorted.sort();
    let mut h: u64 = 0xcbf29ce484222325;
    for item in sorted {
        for b in item.bytes().chain([0]) {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    format!("fallback-{h:016x}")
}

/// The highest attempt number issued to the agent: control state, not
/// history, and never forgotten with an episode.
fn issued_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("recovery-issued.json")
}

/// The highest attempt number issued, zero when none ever was. A file
/// that is there but cannot be read is an error, never zero.
fn load_issued(agent_dir: &Path) -> std::io::Result<u32> {
    match std::fs::read(issued_path(agent_dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

fn store_issued(agent_dir: &Path, n: u32) -> std::io::Result<()> {
    let path = issued_path(agent_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&n)?)?;
    std::fs::rename(&tmp, &path)
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
    /// The attempt this reply answers.
    pub(crate) fn attempt(&self) -> AttemptRef {
        AttemptRef {
            session: self.session.clone(),
            opened_by: self.opened_by.clone(),
            n: self.n,
        }
    }

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

/// What an idle or blind nudge submits: a hint to look at pending work,
/// never the work itself, carrying the attempt's tag.
pub(crate) fn nudge_text(tag: &str) -> String {
    format!(
        "Clank sees you stopped with pending work ({tag}): check it with \
         `clank status` and carry on."
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
            no_transport: crate::cli::resume::NO_EVIDENCE,
            gate,
            obligated: false,
            readable: true,
            fallback: None,
            role: "",
            issued: 0,
        }
    }

    /// Why the episode left the agent to a person, once it has.
    fn told(ep: &Option<Episode>) -> Option<&str> {
        ep.as_ref()
            .filter(|e| e.state == State::Escalated)
            .and_then(|e| e.notice.as_deref())
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

    /// What a decision does besides marking an attempt unanswered for the
    /// nudge history.
    fn acting(fx: &[Effect]) -> Vec<&Effect> {
        fx.iter().filter(|_| true).collect()
    }

    fn submitted(e: Episode, n: u32, started: Option<&str>, now: u64) -> Episode {
        answered(
            e,
            n,
            Answer::Submitted {
                started: started.map(str::to_string),
            },
            &ctx(now, HELPER, None),
        )
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
        assert!(acting(&fx).is_empty());
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
        assert!(acting(&fx).is_empty());
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
        assert!(fx.is_empty());
        assert!(told(&ep).is_some_and(|t| t.contains("needs a person")));
        assert_eq!(ep.unwrap().class, "authentication_failed");
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
        assert!(acting(&fx).is_empty());
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
        let (ep, _) = decide(Some(ep), &s, &ctx(62, HELPER, None));
        assert!(told(&ep).is_some_and(|t| t.contains("did not recover")));
        s.apply(failed("a1", "max_output_tokens"));
        let (ep, fx) = decide(ep, &s, &ctx(63, HELPER, GO));
        assert!(fx.is_empty(), "{fx:?}");
        assert_eq!(ep.as_ref().unwrap().state, State::Escalated);

        s.apply(started("p1"));
        let (ep, fx) = decide(ep, &s, &ctx(80, None, None));
        assert!(acting(&fx).is_empty());
        assert_eq!(
            ep.as_ref().unwrap().state,
            State::Closed,
            "a person's turn is the reset"
        );
        s.apply(failed("p1", "server_error"));
        let (ep, _) = decide(ep, &s, &ctx(90, None, None));
        assert_eq!(ep.as_ref().unwrap().opened_by, "p1");
        assert!(told(&ep).is_some(), "a new failure is told");
    }

    /// A person's turn that fails in the same batch that ended the last
    /// escalation is a new failure, and is told.
    #[test]
    fn a_new_failure_read_with_the_reset_is_told() {
        let mut s = snap([started("t1"), failed("t1", "billing_error")]);
        let (ep, _) = decide(None, &s, &ctx(0, None, None));
        assert!(told(&ep).is_some());
        s.apply(started("p1"));
        s.apply(failed("p1", "billing_error"));
        let (ep, _) = decide(ep, &s, &ctx(10, None, None));
        assert_eq!(ep.as_ref().unwrap().opened_by, "p1");
        assert!(told(&ep).is_some());
    }

    #[test]
    fn an_interruption_is_not_a_failure() {
        let s = snap([started("t1"), ended("t1", Outcome::Interrupted)]);
        assert_eq!(decide(None, &s, &ctx(2400, HELPER, GO)), (None, Vec::new()));
    }

    #[test]
    fn without_a_transport_a_failure_is_told_once_and_left_to_a_person() {
        let s = snap([started("t1"), failed("t1", "server_overloaded")]);
        let (ep, _) = decide(None, &s, &ctx(0, None, None));
        assert!(told(&ep).is_some_and(|t| t.contains("not enabled")));
        let (ep, fx) = decide(ep, &s, &ctx(5, None, GO));
        assert!(acting(&fx).is_empty());
        assert_eq!(ep.unwrap().state, State::Escalated);
    }

    #[test]
    fn a_fatal_class_escalates_without_an_attempt() {
        let s = snap([started("t1"), failed("t1", "authentication_failed")]);
        let (ep, _) = decide(None, &s, &ctx(0, HELPER, None));
        assert!(told(&ep).is_some());
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
        assert!(acting(&fx).is_empty());
        let (ep, fx) = decide(ep, &s, &ctx(60 + ANSWER_WITHIN, HELPER, GO));
        assert!(fx.is_empty(), "{fx:?}");
        assert!(told(&ep).is_some());
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
        let (ep, _) = decide(Some(ep), &s, &ctx(60 + CONFIRM_WITHIN, HELPER, GO));
        assert!(
            told(&ep).is_some_and(|t| t.contains("never seen")),
            "{ep:?}"
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
        assert!(acting(&fx).is_empty());
        assert_eq!(ep.unwrap().state, State::Closed);
    }

    #[test]
    fn a_held_gate_waits_and_an_end_of_need_forgets_the_episode() {
        let s = snap([started("t1"), failed("t1", "server_error")]);
        let ep = decide(None, &s, &ctx(0, HELPER, None)).0;
        let (ep, fx) = decide(ep, &s, &ctx(60, HELPER, Some(Gate::Hold)));
        assert!(acting(&fx).is_empty());
        assert_eq!(ep.as_ref().unwrap().next_at, None, "held: nothing polls");
        let (resumed, _) = decide(ep.clone(), &s, &ctx(90, HELPER, None));
        assert_eq!(
            resumed.unwrap().next_at,
            Some(90 + RETRY_LOOK),
            "a fold with no hold gives the deadline back"
        );
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
        let ep = answered(ep, 1, Answer::Refused, &ctx(61, HELPER, None));
        assert_eq!(ep.state, State::Closed);
        let (ep, fx) = decide(Some(ep), &s, &ctx(5000, HELPER, GO));
        assert!(acting(&fx).is_empty());
        assert_eq!(ep.unwrap().state, State::Closed);
    }

    #[test]
    fn an_indeterminate_answer_escalates_and_is_never_replayed() {
        let s = snap([started("t1"), failed("t1", "server_error")]);
        let ep = decide(None, &s, &ctx(0, HELPER, None)).0;
        let ep = decide(ep, &s, &ctx(60, HELPER, GO)).0.unwrap();
        let ep = answered(ep, 1, Answer::Indeterminate, &ctx(61, HELPER, None));
        assert_eq!(ep.state, State::Escalated);
        let (_, fx) = decide(Some(ep), &s, &ctx(5000, HELPER, GO));
        assert!(acting(&fx).is_empty());
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
            |ep| answered(ep, 1, Answer::Indeterminate, &ctx(61, HELPER, None));
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
        let ep = answered(ep, 1, Answer::Indeterminate, &ctx(61, HELPER, None));
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
        let after = answered(ep.clone(), 2, Answer::Indeterminate, &ctx(61, HELPER, None));
        assert_eq!(after, ep);
    }

    #[test]
    fn an_absent_transport_is_not_an_attempt_but_cannot_wait_forever() {
        let s = snap([started("t1"), failed("t1", "server_error")]);
        let mut ep = decide(None, &s, &ctx(0, HELPER, None)).0.unwrap();
        let mut now = 60;
        for _ in 0..UNAVAILABLE_LIMIT {
            ep = decide(Some(ep), &s, &ctx(now, HELPER, GO)).0.unwrap();
            let n = ep.attempts.last().unwrap().n;
            ep = answered(ep, n, Answer::Unavailable, &ctx(now, HELPER, None));
            now += RETRY_LOOK;
        }
        assert_eq!(
            ep.spent(),
            0,
            "no budget was spent on a transport that was not there"
        );
        assert_eq!(ep.state, State::Escalated);
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
            role: "master".into(),
            work: vec!["continue plan=p".into()],
            blind_nudge: None,
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

    /// The nudge history recorded in `dir`.
    fn nudged(dir: &Path) -> Vec<crate::cli::nudge_log::Record> {
        crate::cli::nudge_log::read(dir)
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
    /// with nothing waiting, and no transport enabled. Left to a person, latched,
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
        assert!(
            waiting_for_you(dir.path())
                .is_some_and(|t| t.contains("server_overloaded") && t.contains("not enabled")),
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
            (Gate::AutoOff, Some(State::Waiting)),
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
            assert_eq!(d.next_due(), None, "{gate:?}: nothing polls");
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
        assert!(waiting_for_you(dir.path()).is_some(), "{acts:?}");
        let ep = load(dir.path()).unwrap();
        assert_eq!(ep.state, State::Escalated);
        assert_eq!(ep.attempts[0].disposition, Disposition::Indeterminate);
        let acts = d.due(10_000, |_, _| Gate::Go);
        assert!(dispatches(&acts).is_empty(), "never dispatched again");
        assert!(
            matches!(&nudged(dir.path())[..], [r] if matches!(r.outcome,
                crate::cli::nudge_log::Outcome::Indeterminate { .. })),
            "the attempt it knew of is history, once: {acts:?}"
        );
        assert!(
            !pending_path(dir.path()).exists(),
            "and nothing is left pending"
        );
        assert_eq!(d.next_due(), None);
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
            role: "master".into(),
            work: vec!["continue plan=p".into()],
            blind_nudge: None,
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
        assert!(waiting_for_you(home.path()).is_some(), "{acts:?}");
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
        touched(&mut d, &rollout, 2);
        assert!(waiting_for_you(dir.path()).is_some());
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
        d.reconcile(
            vec![codex_facts(dir.path(), &first, "session-one", true, false)],
            0,
        );
        assert_eq!(waiting_for_you(dir.path()), None);
        let acts = d.reconcile(
            vec![codex_facts(dir.path(), &second, "session-one", true, false)],
            1,
        );
        assert!(waiting_for_you(dir.path()).is_some(), "{acts:?}");
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
        assert!(waiting_for_you(dir.path()).is_some(), "{acts:?}");
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
        for _ in 0..UNAVAILABLE_LIMIT {
            let acts = d.due(now, |_, _| Gate::Go);
            assert!(dispatches(&acts).is_empty(), "{acts:?}");
            now += RETRY_LOOK;
        }
        assert!(
            waiting_for_you(dir.path()).is_some_and(|t| t.contains("could not be read")),
            "{:?}",
            waiting_for_you(dir.path())
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
    fn with_auto_mode_off_a_failure_waits_with_no_deadline() {
        enable(Tool::Codex, Some(Transport::CodexStart));
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(FAILED, "t1")]);
        let mut d = driver();
        let mut f = codex_facts(dir.path(), &rollout, "session-one", true, false);
        f.auto = false;
        let acts = d.reconcile(vec![f], 0);
        assert!(dispatches(&acts).is_empty(), "{acts:?}");
        assert_eq!(d.next_due(), None, "nothing is scheduled");
        assert!(
            crate::cli::agent_state::load(dir.path())
                .unwrap()
                .suppressors
                .contains(&crate::cli::agent_state::Suppressor::AutoOff)
        );
        d.reconcile(
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                true,
                false,
            )],
            10,
        );
        assert_eq!(
            d.next_due(),
            Some(10 + RETRY_LOOK),
            "auto back on resumes it"
        );
    }

    /// A record replaced under the reader is read again from scratch, not
    /// appended to what it said before.
    #[test]
    fn a_replaced_record_is_read_afresh() {
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
        assert!(waiting_for_you(dir.path()).is_some());
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
        assert!(waiting_for_you(dir.path()).is_some(), "{acts:?}");
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
        assert!(
            waiting_for_you(&dir.path().join("first")).is_some(),
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
            role: "master".into(),
            work: vec!["continue plan=p".into()],
            blind_nudge: None,
            attending: false,
            asking: false,
        };
        let acts = d.reconcile(vec![f], 0);
        forward(&watch, &acts);
        let setup = next_event(&mut rx).await;
        let acts = d.changed(setup, 1);
        assert!(waiting_for_you(home.path()).is_some(), "{acts:?}");
        assert!(acts.contains(&Act::Watch(watch_of(&rollout))), "{acts:?}");
    }

    fn block_storage(dir: &Path) -> PathBuf {
        let blocked = episode_path(dir).with_extension("json.tmp");
        std::fs::create_dir(&blocked).unwrap();
        blocked
    }

    /// An agent of `tool` whose turn failed, armed with `transport`, and
    /// the attempt recovery dispatched for it at 60.
    fn dispatched_attempt(
        tool: Tool,
        transport: Transport,
        first: &[&str],
    ) -> (Driver, tempfile::TempDir, PathBuf, AttemptRef, String) {
        enable(tool, Some(transport));
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("record.jsonl");
        append(&record, first);
        let mut d = driver();
        let name = match tool {
            Tool::Claude => "claude",
            _ => "codex",
        };
        d.reconcile(
            vec![facts(name, dir.path(), &record, "session-one", true, false)],
            0,
        );
        let acts = d.due(60, |_, _| Gate::Go);
        let (attempt, tag) = acts
            .iter()
            .find_map(|a| match a {
                Act::Dispatch { attempt, tag, .. } => Some((attempt.clone(), tag.clone())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{acts:?}"));
        (d, dir, record, attempt, tag)
    }

    fn claude_failed() -> [&'static str; 2] {
        [
            r#"{"type":"user","promptId":"t1","message":{"content":"go"}}"#,
            r#"{"type":"assistant","isApiErrorMessage":true,"error":"overloaded"}"#,
        ]
    }

    fn prompt(id: &str, text: &str) -> String {
        serde_json::json!({"type": "user", "promptId": id, "message": {"content": text}})
            .to_string()
    }

    /// An attempt's answer is its history, once, whatever recovery has
    /// become since. Here the helper's retry was seen starting in the
    /// transcript, so recovery had moved on, before the helper's reply was
    /// read. Its submission is recorded as the helper's report, and nothing
    /// is retried for it.
    #[test]
    fn an_answer_is_history_whatever_recovery_has_become() {
        use crate::cli::nudge_log::{Outcome, Trigger};
        let (mut d, dir, record, attempt, tag) =
            dispatched_attempt(Tool::Claude, Transport::Helper, &claude_failed());
        append(&record, &[&prompt("p9", &wake_text(&tag))]);
        touched(&mut d, &record, 62);
        assert_ne!(load(dir.path()).unwrap().state, State::Dispatched);
        let acts = d.answer(&label(), &attempt, Answer::Submitted { started: None }, 63);
        let recorded = nudged(dir.path());
        assert_eq!(recorded.len(), 1, "{acts:?}");
        assert_eq!(recorded[0].outcome, Outcome::Reported);
        assert_eq!(
            recorded[0].trigger,
            Trigger::Retry {
                class: "overloaded".into(),
                attempt: 1,
                of: retries("overloaded").len() as u32
            }
        );
        assert_eq!(recorded[0].text, wake_text(&tag));
        assert!(dispatches(&acts).is_empty(), "nothing is retried");
        d.answer(&label(), &attempt, Answer::Submitted { started: None }, 64);
        assert_eq!(nudged(dir.path()).len(), 1, "once");

        let (mut d, codex, _record, attempt, _) = dispatched_attempt(
            Tool::Codex,
            Transport::CodexStart,
            &[&line(STARTED, "t1"), &line(FAILED, "t1")],
        );
        d.answer(&label(), &attempt, Answer::Submitted { started: None }, 61);
        assert_eq!(
            nudged(codex.path())[0].outcome,
            Outcome::Submitted,
            "Codex's own answer"
        );
    }

    /// An answer that arrives after a different failure replaced the
    /// episode keeps its own attempt's failure and text.
    #[test]
    fn a_replaced_episode_leaves_its_attempt_its_own_history() {
        let (mut d, dir, record, attempt, tag) =
            dispatched_attempt(Tool::Claude, Transport::Helper, &claude_failed());
        append(
            &record,
            &[
                &prompt("p9", &wake_text(&tag)),
                &prompt("p10", "something else"),
                r#"{"type":"assistant","isApiErrorMessage":true,"error":"rate_limit"}"#,
            ],
        );
        touched(&mut d, &record, 62);
        assert_eq!(load(dir.path()).unwrap().class, "rate_limit");
        let acts = d.answer(&label(), &attempt, Answer::Indeterminate, 63);
        let recorded = nudged(dir.path());
        assert_eq!(recorded.len(), 1, "{acts:?}");
        assert!(matches!(
            &recorded[0].trigger,
            crate::cli::nudge_log::Trigger::Retry { class, .. } if class == "overloaded"
        ));
        assert_eq!(recorded[0].text, wake_text(&tag));
    }

    /// An attempt whose retry was seen starting, then finishing, with no
    /// answer ever coming, is settled as uncertain at its own deadline,
    /// though recovery has nothing left due; then nothing is pending.
    #[test]
    fn a_started_attempt_no_one_answered_is_settled_at_its_own_deadline() {
        let (mut d, dir, record, _, tag) =
            dispatched_attempt(Tool::Claude, Transport::Helper, &claude_failed());
        append(&record, &[&prompt("p9", &wake_text(&tag))]);
        touched(&mut d, &record, 62);
        assert_ne!(load(dir.path()).unwrap().state, State::Dispatched);
        assert_eq!(d.next_due(), Some(60 + ANSWER_WITHIN), "its own deadline");
        let acts = d.due(60 + ANSWER_WITHIN, |_, _| Gate::Go);
        let recorded = nudged(dir.path());
        assert_eq!(recorded.len(), 1, "{acts:?}");
        assert_eq!(recorded[0].text, wake_text(&tag));
        assert!(dispatches(&acts).is_empty());
        assert!(!pending_path(dir.path()).exists());
        assert_eq!(d.next_due(), None, "nothing idles on a deadline");
    }

    /// An agent taken off the roster mid-attempt still has its attempt
    /// settled, in its own directory, at the attempt's deadline.
    #[test]
    fn an_agent_off_the_roster_still_has_its_attempt_settled() {
        let (mut d, dir, _, attempt, _) =
            dispatched_attempt(Tool::Claude, Transport::Helper, &claude_failed());
        assert!(pending_path(dir.path()).exists());
        d.reconcile(Vec::new(), 61);
        let acts = d.due(60 + ANSWER_WITHIN, |_, _| Gate::Go);
        assert!(acts.is_empty(), "{acts:?}");
        assert!(matches!(&nudged(dir.path())[..], [record]
                if record.attempt.ends_with(&format!(":{}", attempt.n))),);
        assert!(!pending_path(dir.path()).exists());
        assert_eq!(d.next_due(), None);
    }

    /// A monitor that restarts before the deadline reloads the attempt, and
    /// a helper's reply that survived the restart is recorded as that
    /// attempt's report, with its original failure and text.
    #[test]
    fn a_restart_keeps_the_attempt_for_a_reply_that_survived_it() {
        let (d, dir, record, attempt, tag) =
            dispatched_attempt(Tool::Claude, Transport::Helper, &claude_failed());
        drop(d);
        write_reply(
            dir.path(),
            &Reply {
                session: attempt.session.clone(),
                opened_by: attempt.opened_by.clone(),
                n: attempt.n,
                answer: ReplyAnswer::Submitted,
                started: None,
            },
        )
        .unwrap();
        let mut d = driver();
        d.reconcile(
            vec![facts(
                "claude",
                dir.path(),
                &record,
                "session-one",
                true,
                false,
            )],
            70,
        );
        let acts = d.replies(71);
        let recorded = nudged(dir.path());
        assert_eq!(recorded.len(), 1, "{acts:?}");
        assert_eq!(
            recorded[0].outcome,
            crate::cli::nudge_log::Outcome::Reported
        );
        assert_eq!(recorded[0].text, wake_text(&tag));
        assert!(matches!(&recorded[0].trigger,
            crate::cli::nudge_log::Trigger::Retry { class, .. } if class == "overloaded"));
        assert!(dispatches(&acts).is_empty());
        assert!(!pending_path(dir.path()).exists());
    }

    /// An agent taken off the roster before the monitor restarted: its
    /// saved attempt is found when the monitor starts, by the agent's own
    /// directory, and settled at its deadline there.
    #[test]
    fn a_restart_finds_an_attempt_whose_agent_left_the_roster() {
        let (d, dir, _, attempt, tag) =
            dispatched_attempt(Tool::Claude, Transport::Helper, &claude_failed());
        drop(d);
        let root = tempfile::tempdir().unwrap();
        let away = root.path().join(label().as_str());
        std::fs::create_dir_all(&away).unwrap();
        std::fs::copy(pending_path(dir.path()), pending_path(&away)).unwrap();
        let mut d = driver();
        d.discover_pending(root.path());
        assert_eq!(d.next_due(), Some(60 + ANSWER_WITHIN));
        let acts = d.due(60 + ANSWER_WITHIN, |_, _| Gate::Go);
        assert!(dispatches(&acts).is_empty(), "nothing is sent again");
        assert!(
            matches!(&nudged(&away)[..], [r] if r.text == wake_text(&tag)
                && r.attempt.ends_with(&format!(":{}", attempt.n))),
            "{:?}",
            nudged(&away)
        );
        assert!(!pending_path(&away).exists());
        assert_eq!(d.next_due(), None);
    }

    /// A helper's reply for an agent no longer on the roster is still read,
    /// from the directory its attempt is pending in, as that attempt's
    /// report.
    #[test]
    fn a_reply_for_an_agent_off_the_roster_is_its_attempts_report() {
        let (mut d, dir, _, attempt, _) =
            dispatched_attempt(Tool::Claude, Transport::Helper, &claude_failed());
        d.reconcile(Vec::new(), 61);
        write_reply(
            dir.path(),
            &Reply {
                session: attempt.session.clone(),
                opened_by: attempt.opened_by.clone(),
                n: attempt.n,
                answer: ReplyAnswer::Submitted,
                started: None,
            },
        )
        .unwrap();
        let acts = d.replies(62);
        assert!(dispatches(&acts).is_empty());
        assert!(matches!(
            &nudged(dir.path())[..],
            [r] if r.outcome == crate::cli::nudge_log::Outcome::Reported
        ));
        assert!(!pending_path(dir.path()).exists());
    }

    /// The row is in the journal before the attempt stops being pending,
    /// so nothing else has to write it; and a stop between the two, which
    /// leaves both, settles again on the next start as one row.
    #[test]
    fn an_attempt_stops_being_pending_only_after_its_row_is_written() {
        let (mut d, dir, _, attempt, _) =
            dispatched_attempt(Tool::Claude, Transport::Helper, &claude_failed());
        let saved = std::fs::read(pending_path(dir.path())).unwrap();
        d.answer(&label(), &attempt, Answer::Submitted { started: None }, 61);
        assert_eq!(nudged(dir.path()).len(), 1, "written by the answer itself");
        assert!(!pending_path(dir.path()).exists());

        let root = tempfile::tempdir().unwrap();
        let away = root.path().join(label().as_str());
        std::fs::create_dir_all(&away).unwrap();
        std::fs::copy(
            crate::cli::nudge_log::path(dir.path()),
            crate::cli::nudge_log::path(&away),
        )
        .unwrap();
        std::fs::write(pending_path(&away), saved.clone()).unwrap();
        let mut d = driver();
        d.discover_pending(root.path());
        d.due(60 + ANSWER_WITHIN, |_, _| Gate::Go);
        assert_eq!(nudged(&away).len(), 1, "one row for the attempt");

        let (mut d, dir, _, attempt, _) =
            dispatched_attempt(Tool::Claude, Transport::Helper, &claude_failed());
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            d.settle_with(
                &label(),
                &attempt,
                |_| crate::cli::nudge_log::Outcome::Submitted,
                61,
                |_, _| panic!("the process stops while writing the row"),
            )
        }));
        assert!(stopped.is_err());
        assert!(
            pending_path(dir.path()).exists(),
            "still pending, for the next start to settle"
        );
        assert!(!pending_path(&away).exists());
    }

    /// A monitor that restarts past an attempt's deadline reads any reply
    /// that survived before deciding none came, in either order the timer
    /// and the reply pass may run, for an agent on the roster or off it:
    /// the reply's own outcome is recorded, once. With no reply, the
    /// attempt is settled as unanswered. Nothing is sent again, and nothing
    /// is left pending.
    #[test]
    fn an_overdue_restart_reads_the_reply_before_deciding_none_came() {
        use crate::cli::nudge_log::Outcome;
        for on_roster in [true, false] {
            for timer_first in [true, false] {
                for reply in [
                    Some(ReplyAnswer::Submitted),
                    Some(ReplyAnswer::Refused),
                    Some(ReplyAnswer::Indeterminate),
                    None,
                ] {
                    let (d, dir, record, attempt, _) =
                        dispatched_attempt(Tool::Claude, Transport::Helper, &claude_failed());
                    drop(d);
                    let root = tempfile::tempdir().unwrap();
                    let agent = root.path().join(label().as_str());
                    std::fs::create_dir_all(&agent).unwrap();
                    std::fs::copy(pending_path(dir.path()), pending_path(&agent)).unwrap();
                    if let Some(answer) = reply {
                        write_reply(
                            &agent,
                            &Reply {
                                session: attempt.session.clone(),
                                opened_by: attempt.opened_by.clone(),
                                n: attempt.n,
                                answer,
                                started: None,
                            },
                        )
                        .unwrap();
                    }
                    let mut d = driver();
                    d.discover_pending(root.path());
                    let facts = match on_roster {
                        true => vec![facts("claude", &agent, &record, "session-one", true, false)],
                        false => Vec::new(),
                    };
                    d.reconcile(facts, 500);
                    let mut acts = Vec::new();
                    if timer_first {
                        acts.extend(d.due(500, |_, _| Gate::Go));
                        acts.extend(d.replies(500));
                    } else {
                        acts.extend(d.replies(500));
                        acts.extend(d.due(500, |_, _| Gate::Go));
                    }
                    let case = format!("roster {on_roster} timer first {timer_first} {reply:?}");
                    let recorded = nudged(&agent);
                    assert_eq!(recorded.len(), 1, "{case}: {recorded:?}");
                    let want_unanswered = matches!(&recorded[0].outcome,
                        Outcome::Indeterminate { reason } if reason == "no answer came");
                    match reply {
                        Some(ReplyAnswer::Submitted) => {
                            assert_eq!(recorded[0].outcome, Outcome::Reported, "{case}")
                        }
                        Some(ReplyAnswer::Refused) => assert!(
                            matches!(recorded[0].outcome, Outcome::Refused { .. }),
                            "{case}"
                        ),
                        Some(ReplyAnswer::Indeterminate) => assert!(
                            matches!(&recorded[0].outcome, Outcome::Indeterminate { reason }
                                if reason != "no answer came"),
                            "{case}"
                        ),
                        None => assert!(want_unanswered, "{case}"),
                    }
                    assert!(dispatches(&acts).is_empty(), "{case}: nothing sent again");
                    assert!(!pending_path(&agent).exists(), "{case}");
                    assert!(!reply_path(&agent).exists(), "{case}: the reply was read");
                }
            }
        }
    }

    /// An attempt nothing ever answered is history when its deadline
    /// passes, as uncertain; an answer after that adds nothing. An attempt
    /// that was answered gains no second row when its start is not seen.
    #[test]
    fn an_unanswered_attempt_is_uncertain_history_once() {
        let (mut d, dir, _rollout, attempt, _) =
            dispatched_attempt(Tool::Claude, Transport::Helper, &claude_failed());
        let acts = d.due(60 + ANSWER_WITHIN, |_, _| Gate::Go);
        let recorded = nudged(dir.path());
        assert_eq!(recorded.len(), 1, "{acts:?}");
        assert!(matches!(
            &recorded[0].outcome,
            crate::cli::nudge_log::Outcome::Indeterminate { reason } if reason == "no answer came"
        ));
        d.answer(&label(), &attempt, Answer::Submitted { started: None }, 200);
        assert_eq!(nudged(dir.path()).len(), 1, "a late answer adds nothing");

        let (mut d, dir, _rollout, attempt, _) =
            dispatched_attempt(Tool::Claude, Transport::Helper, &claude_failed());
        d.answer(&label(), &attempt, Answer::Submitted { started: None }, 61);
        assert_eq!(nudged(dir.path()).len(), 1);
        d.due(61 + CONFIRM_WITHIN, |_, _| Gate::Go);
        assert_eq!(nudged(dir.path()).len(), 1, "no second row");
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
        let acts = d.answer(&label(), &attempt, Answer::Submitted { started: None }, 61);
        assert!(acts.is_empty(), "{acts:?}");
        assert_eq!(
            nudged(dir.path()).len(),
            1,
            "the history is kept through the hold"
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

    /// An escalation that could not be stored is not lost: its decision is
    /// owed, with a deadline of its own, and is made once storage works.
    #[test]
    fn an_escalation_that_could_not_be_stored_is_made_after_the_hold() {
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
        assert_eq!(waiting_for_you(dir.path()), None, "{acts:?}");
        assert_eq!(d.next_due(), Some(RETRY_LOOK));
        std::fs::remove_dir(&blocked).unwrap();
        let acts = d.due(RETRY_LOOK, |_, _| Gate::Go);
        assert!(waiting_for_you(dir.path()).is_some(), "{acts:?}");
        assert_eq!(d.next_due(), None);
        assert!(
            d.due(RETRY_LOOK * 2, |_, _| Gate::Go).is_empty(),
            "told once"
        );
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
        assert_eq!(waiting_for_you(dir.path()), None, "{acts:?}");
        let ep = load(dir.path()).unwrap();
        assert_eq!(
            (ep.kind, ep.opened_by.as_str(), ep.state),
            (Kind::Idle, "t2", State::Waiting),
            "the finished turn is a new, idle, interval"
        );
    }

    /// A held answer for one episode, then a person's turn that fails
    /// differently: the stored episode and its reason are the new
    /// failure's, and the old escalation is never stored.
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
        assert!(
            waiting_for_you(dir.path()).is_some_and(|t| t.contains("billing_error")),
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

    const DURATION: &str = r#"{"type":"system","subtype":"turn_duration","durationMs":5}"#;
    const INTERRUPTED: &str = r#"{"type":"user","promptId":"P","message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#;

    /// Claude closes every turn with `turn_duration`, a failed one too, and
    /// a person's interrupt is a user record of the same turn: the first end
    /// read is the turn's.
    #[test]
    fn claude_turns_end_on_their_first_recorded_end() {
        let read = |lines: &[&str]| {
            let mut r = RecordReader::new(Tool::Claude, "s1");
            let mut s = Snapshot::default();
            for l in lines {
                if let Some(f) = r.read(l) {
                    s.apply(f);
                }
            }
            s.latest().map(|t| t.outcome.clone())
        };
        let go = user("P");
        let error = r#"{"type":"assistant","isApiErrorMessage":true,"error":"overloaded"}"#;
        assert_eq!(read(&[&go, DURATION]), Some(Outcome::Completed));
        assert_eq!(
            read(&[&go, error, DURATION]),
            Some(Outcome::Failed("overloaded".into()))
        );
        assert_eq!(
            read(&[&go, INTERRUPTED, DURATION]),
            Some(Outcome::Interrupted)
        );
        assert_eq!(read(&[&go]), Some(Outcome::Running));
    }

    /// `ctx` for an agent that owes work and has no wait to take it.
    fn owing(now: u64, transport: Option<Transport>, gate: Option<Gate>) -> Ctx<'static> {
        Ctx {
            obligated: true,
            ..ctx(now, transport, gate)
        }
    }

    fn done(id: &str) -> Fact {
        ended(id, Outcome::Completed)
    }

    /// An agent whose turn ended the ordinary way while it owes work is
    /// nudged after its settle period, and only then: not when it owes
    /// nothing, not after a stopped turn, and not while the harness retries.
    #[test]
    fn an_idle_agent_owing_work_is_nudged_after_it_settles() {
        let s = snap([started("t1"), done("t1")]);
        let (ep, fx) = decide(None, &s, &owing(0, HELPER, None));
        assert!(fx.is_empty());
        let ep = ep.unwrap();
        assert_eq!(
            (ep.kind, ep.state, ep.next_at),
            (Kind::Idle, State::Waiting, Some(IDLE_WAITS[0]))
        );
        let (_, fx) = decide(Some(ep.clone()), &s, &owing(IDLE_WAITS[0] - 1, HELPER, GO));
        assert!(fx.is_empty(), "not before it settles");
        let (ep, fx) = decide(Some(ep), &s, &owing(IDLE_WAITS[0], HELPER, GO));
        assert!(
            matches!(fx.as_slice(), [Effect::Dispatch { n: 1, target, .. }] if target == "t1"),
            "{fx:?}"
        );
        assert_eq!(ep.unwrap().state, State::Dispatched);

        assert_eq!(
            decide(None, &s, &ctx(0, HELPER, None)),
            (None, Vec::new()),
            "owes nothing"
        );
        let stopped = snap([started("t1"), ended("t1", Outcome::Interrupted)]);
        assert_eq!(
            decide(None, &stopped, &owing(0, HELPER, None)),
            (None, Vec::new())
        );
        let mut retrying = snap([started("t1"), done("t1")]);
        retrying.apply(Fact::NativeRetry);
        assert_eq!(
            decide(None, &retrying, &owing(0, HELPER, None)),
            (None, Vec::new())
        );
    }

    /// Each suppressor the gate reports holds an idle nudge back, and a
    /// person's turn ends the episode.
    #[test]
    fn suppressors_hold_an_idle_nudge_and_a_new_turn_ends_it() {
        let s = snap([started("t1"), done("t1")]);
        let ep = decide(None, &s, &owing(0, HELPER, None)).0;
        for gate in [Gate::Hold, Gate::Unobserved] {
            let (ep, fx) = decide(ep.clone(), &s, &owing(IDLE_WAITS[0], HELPER, Some(gate)));
            assert!(fx.is_empty(), "{gate:?}");
            assert_eq!(ep.unwrap().state, State::Waiting, "{gate:?}");
        }
        for gone in [Gate::Deliverer, Gate::AutoOff] {
            let (_, fx) = decide(ep.clone(), &s, &owing(IDLE_WAITS[0], HELPER, Some(gone)));
            assert!(fx.is_empty(), "{gone:?}");
        }
        let mut s = s;
        s.apply(started("p1"));
        let (ep, fx) = decide(ep, &s, &owing(IDLE_WAITS[0], HELPER, GO));
        assert!(fx.is_empty());
        assert_eq!(ep.unwrap().state, State::Closed, "a person took it back");
    }

    /// Without a resume capability an idle agent is never nudged: after its
    /// settle period it is left to a person, with the reason.
    #[test]
    fn without_a_capability_an_idle_agent_is_only_explained() {
        let s = snap([started("t1"), done("t1")]);
        let ep = decide(None, &s, &owing(0, None, None)).0;
        assert_eq!(told(&ep), None, "not before it settles");
        let (ep, fx) = decide(ep, &s, &owing(IDLE_WAITS[0], None, GO));
        assert!(fx.is_empty());
        assert!(
            told(&ep).is_some_and(|t| t.contains("idle") && t.contains("not enabled")),
            "{ep:?}"
        );
    }

    /// Dispatch an idle episode's next attempt, have it start as `id`, and
    /// read it ending with `end`.
    fn nudge_round(
        ep: Episode,
        s: &mut Snapshot,
        now: u64,
        id: &str,
        end: Fact,
    ) -> Option<Episode> {
        let (ep, fx) = decide(Some(ep), s, &owing(now, HELPER, GO));
        let Some(Effect::Dispatch { n, .. }) = fx.first() else {
            panic!("no dispatch at {now}: {ep:?}");
        };
        let ep = submitted(ep.unwrap(), *n, Some(id), now);
        s.apply(started(id));
        s.apply(end);
        decide(Some(ep), s, &owing(now + 1, HELPER, None)).0
    }

    /// A nudged turn that ends with the work still owed is idle again, on
    /// the same budget: two nudges, then a person.
    #[test]
    fn an_idle_episode_has_two_nudges_then_a_person() {
        let mut s = snap([started("t1"), done("t1")]);
        let ep = decide(None, &s, &owing(0, HELPER, None)).0.unwrap();
        let ep = nudge_round(ep, &mut s, IDLE_WAITS[0], "a1", done("a1")).unwrap();
        assert_eq!(
            (ep.state, ep.failed_turn.as_str(), ep.next_at),
            (
                State::Waiting,
                "a1",
                Some(IDLE_WAITS[0] + 1 + IDLE_WAITS[1])
            )
        );
        let at = ep.next_at.unwrap();
        let ep = nudge_round(ep, &mut s, at, "a2", done("a2"));
        assert!(
            told(&ep).is_some_and(|t| t.contains("did not get it going")),
            "{ep:?}"
        );
        assert_eq!(ep.unwrap().spent(), 2);
    }

    /// A nudged turn that fails on a provider error stays in its episode:
    /// what was spent stays spent, so the smaller budget left decides.
    #[test]
    fn a_nudged_turn_that_fails_spends_from_the_same_budget() {
        let mut s = snap([started("t1"), done("t1")]);
        let ep = decide(None, &s, &owing(0, HELPER, None)).0.unwrap();
        let ep = nudge_round(
            ep,
            &mut s,
            IDLE_WAITS[0],
            "a1",
            failed("a1", "server_error"),
        )
        .unwrap();
        assert_eq!(
            (ep.kind, ep.opened_by.as_str(), ep.class.as_str(), ep.state),
            (Kind::Idle, "t1", "server_error", State::Waiting)
        );
        assert_eq!(
            ep.next_at,
            Some(IDLE_WAITS[0] + 1 + retries("server_error")[1])
        );
        let at = ep.next_at.unwrap();
        let ep = nudge_round(ep, &mut s, at, "a2", failed("a2", "server_error"));
        assert!(
            told(&ep).is_some(),
            "server_error has a third retry, but the idle budget is spent: {ep:?}"
        );

        let mut s = snap([started("t1"), done("t1")]);
        let ep = decide(None, &s, &owing(0, HELPER, None)).0.unwrap();
        let ep = nudge_round(
            ep,
            &mut s,
            IDLE_WAITS[0],
            "a1",
            failed("a1", "billing_error"),
        );
        assert!(
            told(&ep).is_some_and(|t| t.contains("needs a person")),
            "{ep:?}"
        );
    }

    /// A failure is its own episode, never an idle nudge, whatever its
    /// class; and with one live, the idle path opens nothing beside it.
    #[test]
    fn a_failure_never_becomes_an_idle_nudge() {
        let s = snap([started("t1"), failed("t1", "authentication_failed")]);
        let ep = decide(None, &s, &owing(0, HELPER, None)).0.unwrap();
        assert_eq!((ep.kind, ep.state), (Kind::Failed, State::Escalated));
        let (ep, fx) = decide(Some(ep), &s, &owing(10_000, HELPER, GO));
        assert!(fx.is_empty());
        assert_eq!(ep.unwrap().kind, Kind::Failed);
    }

    /// One attempt in flight per agent: while one is out, a deadline adds
    /// none, whatever else the record shows.
    #[test]
    fn one_attempt_is_in_flight_at_a_time() {
        let s = snap([started("t1"), done("t1")]);
        let ep = decide(None, &s, &owing(0, HELPER, None)).0;
        let (ep, fx) = decide(ep, &s, &owing(IDLE_WAITS[0], HELPER, GO));
        assert_eq!(fx.len(), 1);
        for now in [IDLE_WAITS[0] + 1, IDLE_WAITS[0] + ANSWER_WITHIN - 1] {
            let (_, fx) = decide(ep.clone(), &s, &owing(now, HELPER, GO));
            assert!(fx.is_empty(), "{now}");
        }
    }

    fn blind(identity: &str) -> Ctx<'static> {
        Ctx {
            readable: false,
            fallback: Some(Fallback {
                identity: identity.into(),
                every: 600,
            }),
            ..owing(0, HELPER, None)
        }
    }

    fn at(now: u64, gate: Option<Gate>, ctx: Ctx<'static>) -> Ctx<'static> {
        Ctx { now, gate, ..ctx }
    }

    /// An unreadable record is status-only unless `blind_nudge` is set.
    #[test]
    fn unknown_is_never_nudged_without_the_opt_in() {
        let none = Snapshot::default();
        let unreadable = Ctx {
            readable: false,
            ..owing(0, HELPER, None)
        };
        assert_eq!(decide(None, &none, &unreadable), (None, Vec::new()));
        let ep = decide(None, &none, &blind("w1")).0.unwrap();
        assert_eq!((ep.kind, ep.next_at), (Kind::Fallback, Some(600)));
    }

    /// A blind nudge whose checks find nowhere to send waits its interval,
    /// doubling, and stops after its cap; one that is sent latches, and is
    /// never sent again in that interval.
    #[test]
    fn a_blind_nudge_is_bounded_and_sends_at_most_once() {
        let none = Snapshot::default();
        let mut ep = decide(None, &none, &blind("w1")).0.unwrap();
        let mut waits = Vec::new();
        for _ in 0..FALLBACK_CHECKS {
            let now = ep.next_at.unwrap();
            let (next, fx) = decide(
                Some(ep),
                &none,
                &at(now, Some(Gate::Unobserved), blind("w1")),
            );
            let [Effect::Dispatch { n, .. }] = fx.as_slice() else {
                panic!("{fx:?}");
            };
            ep = answered(
                next.unwrap(),
                *n,
                Answer::Unavailable,
                &at(now, None, blind("w1")),
            );
            waits.push(ep.next_at.map(|t| t - now));
        }
        assert_eq!(
            waits,
            [Some(1200), Some(2400), Some(4800), Some(9600), None],
            "doubling, then a person"
        );
        assert!(told(&Some(ep.clone())).is_some());

        let ep = decide(None, &none, &blind("w1")).0.unwrap();
        let (ep, fx) = decide(Some(ep), &none, &at(600, GO, blind("w1")));
        assert_eq!(fx.len(), 1);
        let ep = answered(
            ep.unwrap(),
            1,
            Answer::Submitted { started: None },
            &at(601, None, blind("w1")),
        );
        assert_eq!(ep.state, State::Escalated, "latched");
        let (ep, fx) = decide(Some(ep), &none, &at(100_000, GO, blind("w1")));
        assert!(fx.is_empty(), "never sent again");
        assert_eq!(ep.as_ref().unwrap().attempts.len(), 1);

        let (ep, _) = decide(ep, &none, &blind("w2"));
        let ep = ep.unwrap();
        assert_eq!(
            (ep.opened_by.as_str(), ep.state, ep.attempts.len()),
            ("w2", State::Waiting, 0),
            "other work is another interval"
        );
        let readable = Ctx {
            readable: true,
            ..blind("w2")
        };
        assert_eq!(
            decide(Some(ep), &none, &readable).0,
            None,
            "a readable record ends it"
        );
    }

    /// The same work names the same interval in any order; other work, or
    /// none, names another.
    #[test]
    fn a_fallback_interval_is_named_by_its_work() {
        let a = fallback_identity(&["x".into(), "y".into()]);
        assert_eq!(a, fallback_identity(&["y".into(), "x".into()]));
        assert_ne!(a, fallback_identity(&["x".into()]));
        assert_ne!(a, fallback_identity(&["xy".into()]));
    }

    /// Claim an idle Codex agent's first nudge, with a typed transport.
    fn claimed_codex() -> (tempfile::TempDir, PathBuf, Driver, AttemptRef) {
        enable(Tool::Codex, Some(Transport::Typed));
        let (dir, record, mut d) = idle_codex();
        let acts = d.due(IDLE_WAITS[0], |_, _| Gate::Go);
        let attempt = dispatches(&acts)[0].0.clone();
        (dir, record, d, attempt)
    }

    fn live(role: &str) -> Live {
        Live {
            role: role.into(),
            work: vec!["continue plan=p".into()],
        }
    }

    /// Whether a claimed attempt may still be sent is decided again from
    /// what the driver knows then: each change since the claim vetoes it.
    #[test]
    fn a_claimed_send_is_vetoed_by_any_change_since_the_claim() {
        let (_dir, _record, mut d, attempt) = claimed_codex();
        assert!(d.may_send(
            &label(),
            &attempt,
            &Live {
                role: "master".into(),
                work: vec!["continue plan=p".into()],
            }
        ));
        for change in [
            "newer turn",
            "auto off",
            "attending",
            "asking",
            "work cleared",
            "deliverer",
            "session",
            "role",
        ] {
            let (dir, record, mut d, attempt) = claimed_codex();
            let mut f = codex_facts(dir.path(), &record, "session-one", true, false);
            match change {
                "newer turn" => append(&record, &[&line(STARTED, "t2")]),
                "auto off" => f.auto = false,
                "attending" => f.attending = true,
                "asking" => f.asking = true,
                "work cleared" => f.obligated = false,
                "deliverer" => f.delivering = true,
                "session" => f = codex_facts(dir.path(), &record, "session-two", true, false),
                _ => f.role = "commit".into(),
            }
            d.reconcile(vec![f], IDLE_WAITS[0] + 1);
            assert!(
                !d.may_send(
                    &label(),
                    &attempt,
                    &Live {
                        role: "master".into(),
                        work: vec!["continue plan=p".into()],
                    }
                ),
                "{change}"
            );
        }
        let (_dir, _record, mut d, attempt) = claimed_codex();
        let other = AttemptRef {
            n: attempt.n + 1,
            ..attempt.clone()
        };
        assert!(
            !d.may_send(&label(), &other, &live("master")),
            "only the claimed attempt"
        );
        let (_dir, _record, mut d, attempt) = claimed_codex();
        assert!(
            !d.may_send(&label(), &attempt, &live("commit")),
            "a role changed on disk"
        );
        let done = Live {
            work: Vec::new(),
            ..live("master")
        };
        assert!(!d.may_send(&label(), &attempt, &done), "work done on disk");
    }

    /// `split-backup-formal`: a Claude master stopped with work owed and
    /// nothing waiting for it. With its capability, it is nudged once after
    /// the settle period, by a typed submit naming the attempt, and the
    /// nudge is in its history as an idle one.
    #[test]
    fn a_stopped_master_owing_work_is_nudged_once() {
        use crate::cli::nudge_log::{Outcome as Said, Trigger};
        enable(Tool::Claude, Some(Transport::Typed));
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        append(&transcript, &[&user("p1"), DURATION]);
        let mut d = driver();
        let f = || {
            vec![facts(
                "claude",
                dir.path(),
                &transcript,
                "session-one",
                true,
                false,
            )]
        };
        let acts = d.reconcile(f(), 0);
        assert!(dispatches(&acts).is_empty(), "{acts:?}");
        assert_eq!(d.next_due(), Some(IDLE_WAITS[0]));
        let acts = d.due(IDLE_WAITS[0], |_, _| Gate::Go);
        let [
            Act::Dispatch {
                attempt,
                transport,
                text,
                tag,
                ..
            },
        ] = acts.as_slice()
        else {
            panic!("{acts:?}");
        };
        assert_eq!(*transport, Transport::Typed);
        assert_eq!(text, &nudge_text(tag));
        let attempt = attempt.clone();
        d.answer(
            &label(),
            &attempt,
            Answer::Submitted { started: None },
            IDLE_WAITS[0] + 1,
        );
        assert!(matches!(
            &nudged(dir.path())[..],
            [r] if r.outcome == Said::Submitted && r.trigger == Trigger::Idle { attempt: 1, of: 2 }
        ));
        append(&transcript, &[&prompt("p2", &nudge_text(tag))]);
        touched(&mut d, &transcript, IDLE_WAITS[0] + 2);
        assert!(
            d.due(IDLE_WAITS[0] + 10_000, |_, _| Gate::Go)
                .iter()
                .all(|a| !matches!(a, Act::Dispatch { .. })),
            "nudged once: its turn is running"
        );
        assert_eq!(load(dir.path()).unwrap().state, State::Running);
    }

    /// The published state: an agent owing nothing is unobserved, never
    /// idle, and has no deadline, no watch and no pane read; one owing work
    /// is read and shown idle, with what holds a nudge back.
    #[test]
    fn the_published_state_never_calls_an_unobserved_agent_idle() {
        use crate::cli::agent_state::{Activity, Nudge, Suppressor, load as published};
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, &[&line(STARTED, "t1"), &line(DONE, "t1")]);
        let mut d = driver();
        let acts = d.reconcile(
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                false,
                true,
            )],
            0,
        );
        assert!(acts.is_empty(), "no watch: {acts:?}");
        assert_eq!(d.next_due(), None);
        let s = published(dir.path()).unwrap();
        assert_eq!(s.activity, Activity::Unobserved);
        assert_eq!(s.suppressors, [Suppressor::Delivering]);
        assert_eq!(
            (s.session.as_str(), s.role.as_str()),
            ("session-one", "master")
        );

        d.reconcile(
            vec![codex_facts(
                dir.path(),
                &rollout,
                "session-one",
                true,
                false,
            )],
            5,
        );
        let s = published(dir.path()).unwrap();
        assert_eq!(
            (s.activity, s.since),
            (Activity::Idle, 5),
            "read before deciding"
        );
        assert_eq!(s.nudge, Nudge::At(5 + IDLE_WAITS[0]));
        assert!(s.cannot_resume.is_some(), "no capability evidence");

        let mut off = codex_facts(dir.path(), &rollout, "session-one", true, false);
        off.auto = false;
        d.reconcile(vec![off], 6);
        let s = published(dir.path()).unwrap();
        assert_eq!(
            (s.activity, s.since),
            (Activity::Idle, 5),
            "since is semantic"
        );
        assert!(s.suppressors.contains(&Suppressor::AutoOff));
    }

    /// The opt-in reaches the driver: an agent owing work whose record
    /// cannot be found is unknown, with no deadline, unless `blind_nudge`
    /// is set, and then its interval is the configured one.
    #[test]
    fn only_the_configured_fallback_schedules_an_unreadable_agent() {
        use crate::cli::agent_state::{Activity, load as published};
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-written.jsonl");
        let mut d = driver();
        d.reconcile(
            vec![codex_facts(
                dir.path(),
                &missing,
                "session-one",
                true,
                false,
            )],
            0,
        );
        assert_eq!(d.next_due(), None);
        assert_eq!(load(dir.path()), None);
        assert!(matches!(
            published(dir.path()).unwrap().activity,
            Activity::Unknown(_)
        ));

        let mut f = codex_facts(dir.path(), &missing, "session-one", true, false);
        f.blind_nudge = Some(600);
        d.reconcile(vec![f], 10);
        assert_eq!(d.next_due(), Some(610));
        assert_eq!(load(dir.path()).unwrap().kind, Kind::Fallback);
    }

    fn idle_codex() -> (tempfile::TempDir, PathBuf, Driver) {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("record.jsonl");
        append(&record, &[&line(STARTED, "t1"), &line(DONE, "t1")]);
        let mut d = driver();
        d.reconcile(
            vec![codex_facts(dir.path(), &record, "session-one", true, false)],
            0,
        );
        assert_eq!(d.next_due(), Some(IDLE_WAITS[0]));
        (dir, record, d)
    }

    /// Manual mode, and a question, block or attendance, each take the
    /// nudge's deadline away at the fold that sees them, not at the deadline.
    #[test]
    fn a_suppressor_takes_the_deadline_away_at_once() {
        for how in ["auto off", "attending", "asking"] {
            let (dir, record, mut d) = idle_codex();
            let mut f = codex_facts(dir.path(), &record, "session-one", true, false);
            match how {
                "auto off" => f.auto = false,
                "attending" => f.attending = true,
                _ => f.asking = true,
            }
            d.reconcile(vec![f], 1);
            assert_eq!(d.next_due(), None, "{how}");
            assert_eq!(load(dir.path()).unwrap().state, State::Waiting, "{how}");
        }
    }

    /// A role change is other work: the old role's nudge is forgotten.
    #[test]
    fn a_role_change_forgets_the_old_roles_nudge() {
        let (dir, record, mut d) = idle_codex();
        let mut f = codex_facts(dir.path(), &record, "session-one", true, false);
        f.role = "commit".into();
        d.reconcile(vec![f], 1);
        let ep = load(dir.path()).unwrap();
        assert_eq!(
            (ep.opened_by.as_str(), ep.next_at),
            ("t1", Some(1 + IDLE_WAITS[0])),
            "a fresh interval for the new role"
        );
    }

    /// A record that cannot be read any more is no longer evidence: the
    /// state says unknown rather than what it last said.
    #[test]
    fn a_failed_read_invalidates_what_was_read() {
        use crate::cli::agent_state::{Activity, load as published};
        let (dir, record, mut d) = idle_codex();
        assert_eq!(published(dir.path()).unwrap().activity, Activity::Idle);
        std::fs::remove_file(&record).unwrap();
        touched(&mut d, &record, 1);
        assert!(
            matches!(
                published(dir.path()).unwrap().activity,
                Activity::Unknown(_)
            ),
            "{:?}",
            published(dir.path())
        );
    }

    /// A record renamed over the old one is another record, even at the
    /// same length: it is read afresh.
    #[test]
    fn an_equal_length_replacement_is_read_afresh() {
        use crate::cli::agent_state::{Activity, load as published};
        let (dir, record, mut d) = idle_codex();
        let len = std::fs::metadata(&record).unwrap().len() as usize;
        let mut bytes = format!("{}\n", line(STARTED, "t2")).into_bytes();
        bytes.resize(len, b' ');
        let other = dir.path().join("replacement.jsonl");
        std::fs::write(&other, bytes).unwrap();
        std::fs::rename(&other, &record).unwrap();
        touched(&mut d, &record, 1);
        assert_eq!(published(dir.path()).unwrap().activity, Activity::Working);
    }

    /// A new binding's state is its own: its age starts when it was bound.
    #[test]
    fn a_new_binding_starts_its_own_age() {
        use crate::cli::agent_state::load as published;
        let (dir, record, mut d) = idle_codex();
        d.reconcile(
            vec![codex_facts(dir.path(), &record, "session-two", true, false)],
            100,
        );
        assert_eq!(published(dir.path()).unwrap().since, 100);
    }

    /// Reset the claimed agent's interval with a role change, and claim
    /// the new interval's first attempt.
    fn reclaimed(d: &mut Driver, dir: &Path, record: &Path, now: u64) -> AttemptRef {
        let mut f = codex_facts(dir, record, "session-one", true, false);
        f.role = "commit".into();
        f.work = vec!["review other work".into()];
        d.reconcile(vec![f], now);
        let acts = d.due(now + IDLE_WAITS[0], |_, _| Gate::Go);
        dispatches(&acts)[0].0.clone()
    }

    /// An attempt's identity is never reused, though a role change resets
    /// the budget on the same turn: each interval's attempts are their own
    /// history rows.
    #[test]
    fn a_reset_interval_never_reuses_an_attempts_identity() {
        let (dir, record, mut d, first) = claimed_codex();
        d.answer(&label(), &first, Answer::Submitted { started: None }, 181);
        let second = reclaimed(&mut d, dir.path(), &record, 200);
        assert_ne!(first, second);
        d.answer(&label(), &second, Answer::Submitted { started: None }, 400);
        assert_eq!(nudged(dir.path()).len(), 2);
    }

    /// An old interval's answer, or its turn, is not the new interval's:
    /// it settles only its own history, and leaves the new claim claimed.
    #[test]
    fn an_old_intervals_answer_or_turn_is_not_the_new_ones() {
        let (dir, record, mut d, old) = claimed_codex();
        let new = reclaimed(&mut d, dir.path(), &record, 181);
        d.answer(&label(), &old, Answer::Submitted { started: None }, 400);
        let ep = load(dir.path()).unwrap();
        assert_eq!(
            ep.attempts.last().unwrap().disposition,
            Disposition::Claimed
        );
        assert_eq!(nudged(dir.path()).len(), 1, "the old attempt's own row");

        let old_attempt = Attempt {
            n: old.n,
            claimed_at: 0,
            transport: "typed".into(),
            target: "t1".into(),
            tag: format!("clank-recovery-t1-{}", old.n),
            started: None,
            disposition: Disposition::Submitted,
        };
        let new_turn = TurnRec {
            id: "x".into(),
            text: Some(nudge_text(&format!("clank-recovery-t1-{}", new.n))),
            outcome: Outcome::Running,
            retrying: false,
        };
        assert!(!old_attempt.made(&new_turn));
        let twelfth = TurnRec {
            text: Some(nudge_text("clank-recovery-t1-12")),
            ..new_turn
        };
        assert!(
            !Attempt {
                n: 1,
                tag: "clank-recovery-t1-1".into(),
                ..old_attempt
            }
            .made(&twelfth)
        );
    }

    /// The identity holds across a restart: after the episode is forgotten
    /// (its work cleared), a new driver continues past what was issued, by
    /// the history and by a claim still pending.
    #[test]
    fn a_restart_never_reuses_an_attempts_identity() {
        let (dir, record, mut d, first) = claimed_codex();
        d.answer(&label(), &first, Answer::Submitted { started: None }, 181);
        let cleared = |d: &mut Driver, now| {
            d.reconcile(
                vec![codex_facts(
                    dir.path(),
                    &record,
                    "session-one",
                    false,
                    false,
                )],
                now,
            );
            assert_eq!(load(dir.path()), None, "forgotten");
        };
        let claim = |d: &mut Driver, now| {
            d.reconcile(
                vec![codex_facts(dir.path(), &record, "session-one", true, false)],
                now,
            );
            let acts = d.due(now + IDLE_WAITS[0], |_, _| Gate::Go);
            dispatches(&acts)[0].0.clone()
        };
        cleared(&mut d, 190);
        drop(d);
        let mut d = driver();
        let second = claim(&mut d, 200);
        assert!(second.n > first.n, "{first:?} {second:?}");
        cleared(&mut d, 400);
        drop(d);
        let mut d = driver();
        let third = claim(&mut d, 500);
        assert!(
            third.n > second.n,
            "a pending claim counts too: {second:?} {third:?}"
        );
    }

    /// An attempt's identity is control state: a history that cannot be
    /// written changes nothing about the numbers issued after a reset.
    #[test]
    fn an_unwritable_history_never_recycles_an_identity() {
        let (dir, record, mut d, first) = claimed_codex();
        std::fs::create_dir(crate::cli::nudge_log::path(dir.path())).unwrap();
        d.answer(&label(), &first, Answer::Submitted { started: None }, 181);
        let second = reclaimed(&mut d, dir.path(), &record, 200);
        assert!(second.n > first.n, "{first:?} {second:?}");
    }

    /// A reset interval's first attempt is the first of its own budget in
    /// the history, whatever number it was issued.
    #[test]
    fn history_counts_each_interval_from_its_own_budget() {
        use crate::cli::nudge_log::Trigger;
        let (dir, record, mut d, first) = claimed_codex();
        d.answer(&label(), &first, Answer::Submitted { started: None }, 181);
        let second = reclaimed(&mut d, dir.path(), &record, 200);
        d.answer(&label(), &second, Answer::Submitted { started: None }, 400);
        let rows = nudged(dir.path());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].trigger, Trigger::Idle { attempt: 1, of: 2 });
        assert_ne!(rows[0].attempt, rows[1].attempt);
    }

    /// An issued number that cannot be read is not zero: nothing is
    /// claimed until it can be.
    #[test]
    fn an_unreadable_issued_number_claims_nothing() {
        enable(Tool::Codex, Some(Transport::Typed));
        let (dir, _record, mut d) = idle_codex();
        std::fs::write(issued_path(dir.path()), "not a number").unwrap();
        let acts = d.due(IDLE_WAITS[0], |_, _| Gate::Go);
        assert!(dispatches(&acts).is_empty(), "{acts:?}");
        std::fs::write(issued_path(dir.path()), "7").unwrap();
        let acts = d.due(IDLE_WAITS[0] + RETRY_LOOK, |_, _| Gate::Go);
        assert_eq!(dispatches(&acts)[0].0.n, 8);
    }

    /// A fault reading the issued number holds new claims, never what is
    /// already known: an answer that arrives during it is kept, and applied
    /// once the number can be read.
    #[test]
    fn an_issued_number_fault_keeps_answers() {
        for answer in [Answer::Submitted { started: None }, Answer::Indeterminate] {
            let (dir, _record, mut d, attempt) = claimed_codex();
            std::fs::write(issued_path(dir.path()), "corrupt").unwrap();
            d.answer(&label(), &attempt, answer.clone(), 181);
            std::fs::write(issued_path(dir.path()), attempt.n.to_string()).unwrap();
            d.due(181 + RETRY_LOOK, |_, _| Gate::Go);
            let want = match answer {
                Answer::Submitted { .. } => Disposition::Submitted,
                _ => Disposition::Indeterminate,
            };
            assert_eq!(
                load(dir.path())
                    .unwrap()
                    .attempts
                    .last()
                    .unwrap()
                    .disposition,
                want,
                "{answer:?}"
            );
        }
    }

    /// A hold is one deadline: folds and record events inside it neither
    /// read the broken number again nor move the hold.
    #[test]
    fn folds_inside_a_hold_do_not_move_it() {
        let (dir, record, mut d, _) = claimed_codex();
        std::fs::write(issued_path(dir.path()), "corrupt").unwrap();
        let f = || vec![codex_facts(dir.path(), &record, "session-one", true, false)];
        d.reconcile(f(), 181);
        let until = d.agents.get(&label()).unwrap().held_until;
        assert_eq!(until, Some(181 + RETRY_LOOK));
        d.reconcile(f(), 182);
        touched(&mut d, &record, 183);
        assert_eq!(d.agents.get(&label()).unwrap().held_until, until);
    }

    /// What changed during a hold is what its end decides from: work
    /// cleared during it forgets the episode; a suppressor holds it with no
    /// deadline.
    #[test]
    fn a_holds_end_decides_from_what_changed_during_it() {
        for change in ["work cleared", "auto off"] {
            enable(Tool::Codex, Some(Transport::Typed));
            let (dir, record, mut d) = idle_codex();
            std::fs::write(issued_path(dir.path()), "corrupt").unwrap();
            let mut f = codex_facts(dir.path(), &record, "session-one", true, false);
            d.reconcile(vec![f.clone()], 1);
            match change {
                "work cleared" => f.obligated = false,
                _ => f.auto = false,
            }
            d.reconcile(vec![f], 2);
            std::fs::write(issued_path(dir.path()), "0").unwrap();
            let acts = d.due(1 + RETRY_LOOK, |_, _| Gate::Go);
            assert!(dispatches(&acts).is_empty(), "{change}: {acts:?}");
            match change {
                "work cleared" => assert_eq!(load(dir.path()), None, "{change}"),
                _ => assert_eq!(d.next_due(), None, "{change}"),
            }
        }
    }

    /// A blind nudge's history counts its checks, those that found nowhere
    /// to send included: its cap is on checks.
    #[test]
    fn fallback_history_counts_its_checks() {
        use crate::cli::nudge_log::Trigger;
        enable(Tool::Codex, Some(Transport::Typed));
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.jsonl");
        let mut f = codex_facts(dir.path(), &missing, "session-one", true, false);
        f.blind_nudge = Some(600);
        let mut d = driver();
        d.reconcile(vec![f], 0);
        for now in [600, 1801] {
            let acts = d.due(now, |_, _| Gate::Go);
            let attempt = dispatches(&acts)[0].0.clone();
            d.answer(&label(), &attempt, Answer::Unavailable, now + 1);
        }
        let rows = nudged(dir.path());
        assert_eq!(
            rows.iter().map(|r| r.trigger.clone()).collect::<Vec<_>>(),
            [
                Trigger::Fallback { attempt: 1 },
                Trigger::Fallback { attempt: 2 }
            ]
        );
    }

    /// The facts of the claimed agent after its role became `commit`, with
    /// that role's own work.
    fn as_reviewer(dir: &Path, record: &Path) -> Facts {
        Facts {
            role: "commit".into(),
            work: vec!["the new role's review".into()],
            ..codex_facts(dir, record, "session-one", true, false)
        }
    }

    fn reviewer_live() -> Live {
        Live {
            role: "commit".into(),
            work: vec!["the new role's review".into()],
        }
    }

    /// A role change seen during a hold, whether the issued number or the
    /// episode cannot be stored, fences the old role's claim at once, and
    /// is applied when the hold ends: the old interval retired, the new
    /// role's armed afresh, and numbers still never reused.
    #[test]
    fn a_role_change_during_a_hold_fences_the_claim_and_is_applied_after() {
        for hold in ["issued number", "episode storage"] {
            let (dir, record, mut d, old) = claimed_codex();
            let repair = match hold {
                "issued number" => {
                    std::fs::write(issued_path(dir.path()), "corrupt").unwrap();
                    None
                }
                _ => Some(block_storage(dir.path())),
            };
            d.reconcile(vec![as_reviewer(dir.path(), &record)], 182);
            assert!(
                d.agents.get(&label()).unwrap().held_until.is_some(),
                "{hold}: held"
            );
            assert!(!d.may_send(&label(), &old, &reviewer_live()), "{hold}");
            match repair {
                None => std::fs::write(issued_path(dir.path()), old.n.to_string()).unwrap(),
                Some(blocked) => std::fs::remove_dir(blocked).unwrap(),
            }
            d.due(182 + RETRY_LOOK, |_, _| Gate::Go);
            let ep = load(dir.path()).unwrap();
            assert_eq!(
                (ep.role.as_str(), ep.state, ep.attempts.len()),
                ("commit", State::Waiting, 0),
                "{hold}: the new role's interval"
            );
            let acts = d.due(ep.next_at.unwrap(), |_, _| Gate::Go);
            let new = dispatches(&acts)[0].0.clone();
            assert!(new.n > old.n, "{hold}: {old:?} {new:?}");
        }
    }

    /// A restarted driver never adopts another role's claim: the episode
    /// carries the role it was opened under.
    #[test]
    fn a_restart_never_adopts_another_roles_claim() {
        let (dir, record, d, old) = claimed_codex();
        drop(d);
        let mut d = driver();
        d.reconcile(vec![as_reviewer(dir.path(), &record)], 181);
        assert!(!d.may_send(&label(), &old, &reviewer_live()));
        assert_eq!(load(dir.path()).unwrap().role, "commit");
    }
}
