//! The turn-end policy: one pure state machine per (repo, agent, session
//! binding) that decides what every Stop, attendance request, check-in,
//! delivery result and recovery question means.
//!
//! The provider hooks report evidence and render [`Verdict`]s; they decide
//! nothing (the-stop-hook-is-a-messenger). Every transition is a function of
//! the state, the event and [`Facts`] the monitor gathers when it evaluates.
//! Facts are never cached: a proof is re-checked each time it is used.
//!
use serde::{Deserialize, Serialize};

use crate::hook_io::{BackgroundTask, HookInput, clank_run_desc, clank_wait};
use crate::ids::SessionId;
use crate::vocab::{AutoMode, Role, Tool};

/// Milliseconds since the Unix epoch.
pub type Ms = u64;

/// What the provider said about its background work at one Stop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "snapshot", content = "tasks", rename_all = "snake_case")]
pub enum TaskSnapshot {
    /// The provider sent `background_tasks`, possibly empty: a task missing
    /// from it is not running.
    Known(Vec<BackgroundTask>),
    /// The provider sends no task list (codex). Evidence of neither
    /// completion nor liveness.
    Unavailable,
}

impl TaskSnapshot {
    /// The snapshot a Stop's payload carries: a list when the provider
    /// sent one, however empty, and `Unavailable` when it omitted it.
    pub fn of(input: &HookInput) -> Self {
        match &input.background_tasks {
            Some(tasks) => Self::Known(tasks.clone()),
            None => Self::Unavailable,
        }
    }

    /// The tasks the snapshot lists as still running.
    pub fn running(&self) -> Vec<&BackgroundTask> {
        self.live().map(Iterator::collect).unwrap_or_default()
    }

    fn live(&self) -> Option<impl Iterator<Item = &BackgroundTask>> {
        match self {
            Self::Known(tasks) => Some(
                tasks
                    .iter()
                    .filter(|t| t.status.as_deref() != Some("completed")),
            ),
            Self::Unavailable => None,
        }
    }
}

/// Where a Stop sits in the agent's own record, for repeat counting.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnPlace {
    /// The newest turn in the agent's record, where the adapter names one.
    pub turn: Option<String>,
    /// The offset of the newest turn-boundary record. `None` where the
    /// adapter keeps no such record, and then each delivery counts.
    pub position: Option<u64>,
    /// The newest turn is positively a person's.
    pub human_turn: bool,
}

/// One Stop, as the hook observed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEndReport {
    pub tool: Tool,
    pub session: SessionId,
    pub place: TurnPlace,
    pub tasks: TaskSnapshot,
}

/// Work the generic owed-work fold says this agent is owed now.
/// A pending block has already withheld what it pauses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwedItem {
    /// The item's typed identity, as repeat counting compares it.
    pub identity: String,
    /// Receipt ids committed when a presentation carrying it is delivered.
    pub receipts: Vec<String>,
}

/// A shell task's process, as recorded when it was attended: a pid alone
/// is reused, so the birth token says which process it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellProof {
    pub pid: i32,
    pub token: String,
}

/// Whether a recorded process is still that process. The monitor answers
/// it at evaluation time; nothing caches the answer.
pub trait Liveness {
    fn holds(&self, proof: &ShellProof) -> bool;
}

/// Facts the monitor gathers at the moment it evaluates an event.
pub struct Facts<'a> {
    pub now: Ms,
    /// The session currently bound to the label.
    pub binding: Option<&'a SessionId>,
    /// The effective auto mode; `Err` when the config cannot be read.
    pub auto: Result<AutoMode, String>,
    /// The agent's roster role; `Err` when it cannot be resolved.
    pub role: Result<Role, String>,
    /// A question to the user is pending in the agent's session.
    pub asking: bool,
    /// An unanswered block is pending for this agent.
    pub blocked: bool,
    pub owed: &'a [OwedItem],
    pub liveness: &'a dyn Liveness,
    /// A person's signed command, verified just now, waits for this agent
    /// to sit idle and only a park stands in its way.
    pub user_command: bool,
}

/// What an attendance names before it is resolved to a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Selector {
    /// A task's `description`, matched exactly: a subagent's name.
    Name(String),
    /// The `--desc` of a `clank run`.
    RunDesc(String),
    /// A task id, as an older clank's `attend <task> --pid` recorded it.
    /// Kept only so state saved then still resolves; nothing asks for one
    /// now.
    TaskId(String),
}

impl Selector {
    fn matches(&self, task: &BackgroundTask) -> bool {
        match self {
            Self::Name(name) => task.description.as_deref() == Some(name),
            Self::TaskId(id) => task.id.as_deref() == Some(id),
            Self::RunDesc(desc) => task
                .command
                .as_deref()
                .and_then(clank_run_desc)
                .is_some_and(|d| &d == desc),
        }
    }

    pub fn text(&self) -> &str {
        match self {
            Self::Name(text) | Self::RunDesc(text) | Self::TaskId(text) => text,
        }
    }
}

/// The kinds of task an attendance can be proven against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskKind {
    /// The provider's own agent: its listing is the proof.
    Subagent,
    /// A background shell: the listing AND its process must hold.
    Shell,
    /// Anything else, such as a `monitor`. Never proves anything.
    Unknown(Option<String>),
}

impl TaskKind {
    /// No observed payload has carried a shell's `type`, so a task with a
    /// command line and no kind is a shell too: today's records are.
    pub fn of(task: &BackgroundTask) -> Self {
        match task.kind.as_deref() {
            Some("subagent") => Self::Subagent,
            Some("shell") => Self::Shell,
            None if task.command.is_some() => Self::Shell,
            other => Self::Unknown(other.map(str::to_owned)),
        }
    }
}

/// An entry's one check-in. It is armed only once the entry has qualified
/// at a turn-end: one that never qualified quieted nothing, so no wake was
/// handed to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckIn {
    Unarmed,
    Armed,
    Fired,
    /// Due while a gate held it back. Spent, not deferred: what lifts the
    /// gate (the person's answer, auto turning on) is itself a wake, and
    /// past the deadline the entry no longer quiets recovery.
    Withheld(Withheld),
    /// Fired, and the route could not carry it. Never counted as
    /// delivered; past the deadline recovery treats the agent as any
    /// other.
    Undeliverable(String),
    /// Due after its process had already gone: there was nothing left to
    /// check in on.
    ProcessGone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Withheld {
    AutoOff,
    Asking,
    Blocked,
}

/// Which entry a check-in was fired for, and the moment its clock started:
/// a renewal restarts the clock, so a result for the old one is told apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckInRef {
    pub key: EntryKey,
    pub since: Ms,
}

/// Whether the facts now allow the agent to be woken by clank at all.
/// An unreadable config or role cannot establish auto-off, so it fails
/// toward noise and allows the wake.
pub fn wake_gate(facts: &Facts<'_>) -> Result<(), Withheld> {
    if matches!(facts.auto, Ok(AutoMode::Off)) {
        return Err(Withheld::AutoOff);
    }
    if facts.blocked {
        return Err(Withheld::Blocked);
    }
    if facts.asking {
        return Err(Withheld::Asking);
    }
    Ok(())
}

/// The task an entry resolved to: from then on it covers exactly that task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolved {
    pub task_id: String,
    pub kind: TaskKind,
}

/// What every mutation of an entry is scoped to.
///
/// A run is its process: its `--desc` only correlates it with a listing.
/// A subagent is its name until a listing resolves it, then its task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKey {
    Name(String),
    Task(String),
    Run(ShellProof),
}

/// One attended piece of background work, with its own expectation and
/// check-in. Its clock runs from `since`, the moment it was accepted or
/// last renewed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub selector: Selector,
    /// `None` until a turn-end's listing shows exactly one live match.
    pub resolved: Option<Resolved>,
    pub desc: Option<String>,
    pub since: Ms,
    pub expect: Ms,
    pub shell: Option<ShellProof>,
    pub check_in: CheckIn,
}

impl Entry {
    pub fn key(&self) -> EntryKey {
        match (&self.shell, &self.resolved) {
            (Some(proof), _) => EntryKey::Run(proof.clone()),
            (None, Some(r)) => EntryKey::Task(r.task_id.clone()),
            (None, None) => EntryKey::Name(self.selector.text().to_string()),
        }
    }

    pub fn task(&self) -> Option<&str> {
        self.resolved.as_ref().map(|r| r.task_id.as_str())
    }

    pub fn deadline(&self) -> Ms {
        self.since.saturating_add(self.expect)
    }

    fn is_run(&self) -> bool {
        matches!(self.selector, Selector::RunDesc(_))
    }

    /// A new expectation from `since`. A check-in it already earned is
    /// re-armed against the new deadline; one never armed waits for a
    /// qualifying turn-end as before.
    fn renew(&mut self, since: Ms, expect: Ms, desc: Option<String>) {
        self.since = since;
        self.expect = expect;
        if desc.is_some() {
            self.desc = desc;
        }
        if self.check_in != CheckIn::Unarmed {
            self.check_in = CheckIn::Armed;
        }
    }
}

impl Uncovered {
    /// The identity its demand is presented and repeat-counted under,
    /// beside owed work's.
    pub fn identity(&self) -> String {
        format!("uncovered:{}", self.task_id)
    }
}

/// The provider wakes the agent when its background work completes.
/// Without that wake, quiet on an attendance would leave the agent with
/// no wake at all.
pub fn completion_wake(tool: Tool) -> bool {
    matches!(tool, Tool::Claude)
}

/// Why an entry does or does not quiet anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Qualifies {
    Yes,
    /// A known listing no longer has the task: the work is over.
    Ended,
    No(Unqualified),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unqualified {
    NotResolved,
    NoCompletionWake,
    /// No fresh known listing correlates the task.
    Uncorrelated,
    NoProcessProof,
    ProcessGone,
    UnknownKind,
    PastDeadline,
}

/// THE rule. Turn-end, check-in, coverage and recovery all ask this and
/// nothing else whether an entry may quiet anything.
pub fn qualifies(
    entry: &Entry,
    tool: Tool,
    snapshot: Option<&TaskSnapshot>,
    facts: &Facts<'_>,
) -> Qualifies {
    let Some(Resolved { task_id, kind }) = &entry.resolved else {
        return Qualifies::No(Unqualified::NotResolved);
    };
    let Some(mut live) = snapshot.and_then(TaskSnapshot::live) else {
        return Qualifies::No(Unqualified::Uncorrelated);
    };
    if !live.any(|t| t.id.as_deref() == Some(task_id.as_str())) {
        return Qualifies::Ended;
    }
    if !completion_wake(tool) {
        return Qualifies::No(Unqualified::NoCompletionWake);
    }
    match kind {
        TaskKind::Subagent => {}
        TaskKind::Shell => match &entry.shell {
            None => return Qualifies::No(Unqualified::NoProcessProof),
            Some(proof) if !facts.liveness.holds(proof) => {
                return Qualifies::No(Unqualified::ProcessGone);
            }
            Some(_) => {}
        },
        TaskKind::Unknown(_) => return Qualifies::No(Unqualified::UnknownKind),
    }
    if facts.now >= entry.deadline() {
        return Qualifies::No(Unqualified::PastDeadline);
    }
    Qualifies::Yes
}

/// A live task no qualifying entry covers, that a permitted command could.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uncovered {
    pub task_id: String,
    pub work: Attendable,
    /// Why the entry resolved to this task does not cover it, if one is.
    pub entry: Option<Unqualified>,
}

/// Live work, with the handle a permitted command attends it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attendable {
    /// A subagent whose name resolves to it alone.
    Subagent { name: String },
    /// Shell work, and the `--desc` of the `clank run` it is, if it is one.
    Shell { run: Option<String> },
}

/// How many times in a row the same items have been delivered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepeatRecord {
    /// The delivered items' identities, sorted and deduplicated.
    pub items: Vec<String>,
    pub count: u32,
    pub turn: Option<String>,
    /// The furthest record position a counted delivery came from.
    pub high: Option<u64>,
}

/// The delivery at which the message escalates.
pub const ESCALATE_AT: u32 = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepeatDecision {
    /// A new delivery, and the record it commits.
    Counted(RepeatRecord),
    /// The same Stop for the same items, already counted.
    Duplicate(u32),
    /// Placed at or before a counted Stop, for other items: overtaken.
    Superseded,
    /// Not a Stop's continuation (a session start's catch-up): it states
    /// and commits no count.
    Uncounted,
}

impl RepeatDecision {
    pub fn count(&self) -> u32 {
        match self {
            Self::Counted(r) => r.count,
            Self::Duplicate(n) => *n,
            Self::Superseded | Self::Uncounted => 0,
        }
    }
}

/// Today's `wake_repeat` rule, unchanged in meaning: a human turn resets
/// the count, and a Stop placed at or before one already counted is
/// not counted again.
pub fn count_delivery(
    prev: Option<&RepeatRecord>,
    items: &[String],
    place: &TurnPlace,
) -> RepeatDecision {
    let mut items = items.to_vec();
    items.sort();
    items.dedup();
    if let (Some(p), Some(high), Some(here)) = (prev, prev.and_then(|p| p.high), place.position)
        && here <= high
    {
        return if p.items == items {
            RepeatDecision::Duplicate(p.count)
        } else {
            RepeatDecision::Superseded
        };
    }
    let same = prev.filter(|p| p.items == items && !(place.human_turn && p.turn != place.turn));
    RepeatDecision::Counted(RepeatRecord {
        items,
        count: same.map_or(1, |p| p.count.saturating_add(1)),
        turn: place.turn.clone(),
        high: place.position.max(prev.and_then(|p| p.high)),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentationId {
    pub session: SessionId,
    pub seq: u64,
}

/// A delivery chosen but not yet confirmed emitted. It commits nothing
/// until the hook reports [`Event::Delivered`]. An id names exactly one
/// payload: its receipts and repeat decision are fixed when it is made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presentation {
    pub id: PresentationId,
    pub identities: Vec<String>,
    pub receipts: Vec<String>,
    pub repeat: RepeatDecision,
}

/// Text a verdict carries that is not work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// An intent matched no live task, listed so the agent can pick one.
    NoMatch {
        selector: Selector,
        live: Vec<TaskSummary>,
    },
    Ambiguous {
        selector: Selector,
        matches: Vec<TaskSummary>,
    },
    /// A name matched only shell work, whose description is the tool's and
    /// not a handle: shell work is attended by launching it with
    /// `clank run`.
    ShellNamed { name: String, task_id: String },
    /// The Stop listed live work no qualifying entry covers.
    Uncovered(Vec<Uncovered>),
    /// Alone mode: work is owed and no monitor can present it.
    MonitorUnavailable,
    /// Grok's native handoff: run `clank wait` in the background, whose
    /// completion wakes the agent.
    ArmWait,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSummary {
    pub id: Option<String>,
    pub kind: TaskKind,
    pub description: Option<String>,
    /// The `--desc` of the `clank run` the task is, if it is one.
    pub run: Option<String>,
}

impl From<&BackgroundTask> for TaskSummary {
    fn from(t: &BackgroundTask) -> Self {
        Self {
            id: t.id.clone(),
            kind: TaskKind::of(t),
            description: t.description.clone(),
            run: t.command.as_deref().and_then(clank_run_desc),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SilentWhy {
    StaleBinding,
    AutoOff,
    /// A native handoff's wait already carries the agent's next wake.
    WaitArmed,
    /// A person's command waits for an idle prompt, so the turn ends
    /// instead of parking.
    UserCommand,
}

/// What the hook is told to do with one Stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Silent(SilentWhy),
    /// Hold the owned wait; a terminal verdict follows on it.
    Park,
    Deliver {
        presentation: PresentationId,
        identities: Vec<String>,
        repeat: u32,
        escalated: bool,
        notices: Vec<Notice>,
    },
    /// Wake with text that is not work, never empty. Creates no
    /// presentation.
    Notify(Vec<Notice>),
    Failed(String),
}

/// What the hook does when its own deadline arrives while parked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeadlineVerdict {
    /// Wake with a heartbeat so the next Stop parks under a new ceiling.
    ReArm,
    /// Renew the long-poll without a model turn (OpenCode's plugin).
    RenewWait,
    Failed(String),
}

pub fn deadline_verdict(tool: Tool) -> DeadlineVerdict {
    match tool {
        Tool::Claude | Tool::Codex => DeadlineVerdict::ReArm,
        Tool::OpenCode => DeadlineVerdict::RenewWait,
        Tool::Grok => DeadlineVerdict::Failed(
            "hook: Grok native handoffs do not use the in-hook wait route".into(),
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttendRequest {
    Attend {
        session: SessionId,
        selector: Selector,
        desc: Option<String>,
        expect: Ms,
        shell: Option<ShellProof>,
    },
    Clear {
        session: SessionId,
        which: Clearing,
    },
}

/// Which entries a clear removes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Clearing {
    /// Every entry: only the operator's own `clank attend --clear`.
    #[default]
    All,
    /// The entry whose subagent name or run `--desc` this is.
    Named(String),
    /// Exactly the run this process proof registered.
    Run(ShellProof),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttendReply {
    /// A new entry, proven at the next turn-end.
    Accepted,
    /// An entry already attending this work now runs on the new
    /// expectation.
    Renewed,
    Cleared {
        removed: usize,
    },
    /// The name is both a subagent's and a run's: these are the entries
    /// it names, and nothing changed.
    Ambiguous(Vec<Selector>),
    /// Another `clank run` already attended holds this `--desc`, so a
    /// second would make neither correlatable.
    DescInUse(String),
    RefusedStale,
}

/// A side effect the monitor performs for a transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    CommitReceipts(Vec<String>),
    CommitRepeat(Option<RepeatRecord>),
    /// Deliver one check-in, for every entry due together, through the
    /// nudge transport.
    CheckIn(Vec<CheckInLine>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckInLine {
    pub check_in: CheckInRef,
    pub task_id: String,
    pub kind: TaskKind,
    pub desc: Option<String>,
    pub selector: Selector,
    pub elapsed: Ms,
    pub expect: Ms,
}

/// How a presentation's one result settled it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    Delivered,
    Failed,
}

/// What a delivery result did.
///
/// A result is connection-scoped: only the connection that carried a
/// presentation can answer for it, and only while it is in flight. So a
/// failure is terminal. Once the hook reports an emit error, or its
/// connection closes without a result, no success for that presentation
/// can ever arrive. Delivery to the agent is at-least-once, and receipts
/// are exactly-once because only a success on the live connection
/// commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultOutcome {
    Resolved(Vec<Effect>),
    /// The same result again for the presentation it last settled.
    Duplicate,
    /// A result for a presentation that is not in flight, and that this
    /// result did not just settle. The transport makes this impossible,
    /// so it is refused and commits nothing.
    ProtocolViolation(PresentationId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A Stop. A report answered Park is stepped again with the same
    /// report when the state or facts change.
    TurnEnd(TurnEndReport),
    Attend(AttendRequest),
    /// Only from the connection that carried the in-flight presentation.
    Delivered(PresentationId),
    /// The emit failed, or the hook's connection closed without a result:
    /// the monitor turns a closed connection into this, so every
    /// presentation resolves. Terminal: no later success can exist.
    EmitFailed(PresentationId),
    CheckInDue,
    /// The check-in fired for these entries could not be delivered, and
    /// why.
    CheckInUndeliverable {
        check_ins: Vec<CheckInRef>,
        why: String,
    },
    /// A report answered Park, stepped again because the state or facts
    /// changed. It is not a new Stop: it resolves no intent and proves no
    /// attendance, since its listing is as old as the park.
    Parked(TurnEndReport),
    /// A session started: its owed work, presented once and never counted
    /// as a repeat, or nothing.
    SessionStart(SessionId),
    /// The facts changed with no event of the agent's own: an owed-work
    /// change still ends the repeat episode.
    Observed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Verdict(Verdict),
    Attend(AttendReply),
    Effects(Vec<Effect>),
    Result(ResultOutcome),
}

/// What survives a monitor restart. The last task listing does not: a
/// restored entry is unproven until the next turn-end reports one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "legacy::Stored")]
pub struct Persisted {
    pub binding: SessionId,
    pub attendances: Vec<Entry>,
    pub repeat: Option<RepeatRecord>,
    pub outstanding: Option<Presentation>,
    pub outstanding_episode_broken: bool,
    pub resolved: Option<(PresentationId, Resolution)>,
    pub next_seq: u64,
    /// State saved before receipt evidence was kept has none, so nothing
    /// it owes is quiet.
    pub received: Vec<String>,
    pub unwatched: Option<Unwatched>,
}

/// The persisted shapes from before an agent could attend several things,
/// read so a monitor restarted across the upgrade keeps what was attended.
mod legacy {
    use super::*;

    #[derive(Deserialize)]
    pub(super) struct Stored {
        binding: SessionId,
        #[serde(default)]
        attendances: Option<Vec<Entry>>,
        #[serde(default)]
        attendance: Option<Attendance>,
        repeat: Option<RepeatRecord>,
        outstanding: Option<Presentation>,
        #[serde(default)]
        outstanding_episode_broken: bool,
        resolved: Option<(PresentationId, Resolution)>,
        next_seq: u64,
        #[serde(default)]
        received: Vec<String>,
        #[serde(default)]
        unwatched: Option<Unwatched>,
    }

    #[derive(Deserialize)]
    enum Selector {
        TaskId(String),
        Name(String),
        RunDesc(String),
    }

    #[derive(Deserialize)]
    struct Request {
        selector: Selector,
        desc: Option<String>,
        since: Ms,
        expect: Ms,
        shell: Option<ShellProof>,
    }

    #[derive(Deserialize)]
    enum Attendance {
        None,
        Intent(Request),
        Standing {
            task_id: String,
            kind: TaskKind,
            desc: Option<String>,
            since: Ms,
            expect: Ms,
            shell: Option<ShellProof>,
            check_in: CheckIn,
            #[serde(default)]
            renewal: Option<Request>,
        },
    }

    impl From<Stored> for Persisted {
        fn from(s: Stored) -> Self {
            Persisted {
                binding: s.binding,
                attendances: s
                    .attendances
                    .unwrap_or_else(|| s.attendance.map(entries).unwrap_or_default()),
                repeat: s.repeat,
                outstanding: s.outstanding,
                outstanding_episode_broken: s.outstanding_episode_broken,
                resolved: s.resolved,
                next_seq: s.next_seq,
                received: s.received,
                unwatched: s.unwatched,
            }
        }
    }

    fn entries(old: Attendance) -> Vec<Entry> {
        match old {
            Attendance::None => Vec::new(),
            Attendance::Intent(request) => vec![request.entry()],
            Attendance::Standing {
                task_id,
                kind,
                desc,
                since,
                expect,
                shell,
                check_in,
                renewal,
            } => {
                // The old record kept no selector: a subagent was attended
                // by the name it recorded as its description, and a run by
                // its `--desc`.
                let named = desc.clone().unwrap_or_else(|| task_id.clone());
                let selector = match kind {
                    TaskKind::Shell => super::Selector::RunDesc(named),
                    TaskKind::Subagent | TaskKind::Unknown(_) => super::Selector::Name(named),
                };
                let mut standing = Entry {
                    selector,
                    resolved: Some(Resolved { task_id, kind }),
                    desc,
                    since,
                    expect,
                    shell,
                    check_in,
                };
                match renewal {
                    None => vec![standing],
                    Some(request) if request.names(&standing) => {
                        standing.renew(request.since, request.expect, request.desc);
                        vec![standing]
                    }
                    Some(request) => vec![standing, request.entry()],
                }
            }
        }
    }

    impl Request {
        fn entry(self) -> Entry {
            let selector = match self.selector {
                Selector::TaskId(id) => super::Selector::TaskId(id),
                Selector::Name(name) => super::Selector::Name(name),
                Selector::RunDesc(desc) => super::Selector::RunDesc(desc),
            };
            Entry {
                selector,
                resolved: None,
                desc: self.desc,
                since: self.since,
                expect: self.expect,
                shell: self.shell,
                check_in: CheckIn::Unarmed,
            }
        }

        fn names(&self, standing: &Entry) -> bool {
            match &self.selector {
                Selector::TaskId(id) => standing.task() == Some(id.as_str()),
                // A name may since have passed to another task: only a
                // listing can say it is still this one.
                Selector::Name(_) => false,
                Selector::RunDesc(desc) => {
                    self.shell == standing.shell
                        && matches!(&standing.selector, super::Selector::RunDesc(d) if d == desc)
                }
            }
        }
    }
}

/// The current park ended without delivering, at `since`, when the agent's
/// record (by inode) had been read to `read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unwatched {
    pub since: Ms,
    pub record: u64,
    pub read: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionState {
    pub binding: SessionId,
    /// Everything the agent attends. Each entry is independent: adding,
    /// renewing, ending or clearing one never touches another.
    pub attendances: Vec<Entry>,
    /// The latest report's listing, which recovery judges by.
    pub snapshot: Option<TaskSnapshot>,
    /// The tool of the latest report.
    pub tool: Option<Tool>,
    pub repeat: Option<RepeatRecord>,
    /// The one presentation in flight. While it is, no other is made: its
    /// result commits against the record it was counted from, as today's
    /// serialised sends do.
    pub outstanding: Option<Presentation>,
    /// An owed-work change was observed while `outstanding` was in flight,
    /// ending the episode its repeat decision belongs to. Its success still
    /// commits its receipts; it must not bring that count back.
    pub outstanding_episode_broken: bool,
    /// The presentation the last result settled, so a repeat of that
    /// result is told apart from a result no connection could send.
    pub resolved: Option<(PresentationId, Resolution)>,
    pub next_seq: u64,
    /// The identities of owed work a presentation on this binding was
    /// `Delivered` with, sorted: the only work a qualifying entry may
    /// quiet. Never wider than what is still owed.
    pub received: Vec<String>,
    pub unwatched: Option<Unwatched>,
}

impl SessionState {
    pub fn new(binding: SessionId) -> Self {
        Self {
            binding,
            attendances: Vec::new(),
            snapshot: None,
            tool: None,
            repeat: None,
            outstanding: None,
            outstanding_episode_broken: false,
            resolved: None,
            next_seq: 0,
            received: Vec::new(),
            unwatched: None,
        }
    }

    pub fn persisted(&self) -> Persisted {
        Persisted {
            binding: self.binding.clone(),
            attendances: self.attendances.clone(),
            repeat: self.repeat.clone(),
            outstanding: self.outstanding.clone(),
            outstanding_episode_broken: self.outstanding_episode_broken,
            resolved: self.resolved.clone(),
            next_seq: self.next_seq,
            received: self.received.clone(),
            unwatched: self.unwatched,
        }
    }

    /// The connection that carried an in-flight presentation died with
    /// the old monitor, so it is restored as terminally failed. Its work
    /// stays owed and the next stop presents it fresh.
    pub fn restored(p: Persisted) -> Self {
        let resolved = match &p.outstanding {
            Some(lost) => Some((lost.id.clone(), Resolution::Failed)),
            None => p.resolved,
        };
        let mut received = p.received;
        received.sort();
        received.dedup();
        Self {
            binding: p.binding,
            attendances: p.attendances,
            snapshot: None,
            tool: None,
            repeat: p.repeat,
            outstanding: None,
            outstanding_episode_broken: false,
            resolved,
            next_seq: p.next_seq,
            received,
            unwatched: p.unwatched,
        }
    }

    /// A new binding owns nothing of the old one's: its attendances,
    /// outstanding presentation, repeat count and received work all
    /// described another session.
    pub fn rebind(&mut self, binding: SessionId) {
        if self.binding != binding {
            *self = Self::new(binding);
        }
    }

    /// The nearest moment this state needs the monitor to act: the
    /// soonest armed check-in over every entry.
    pub fn next_deadline(&self) -> Option<Ms> {
        self.attendances
            .iter()
            .filter(|e| e.check_in == CheckIn::Armed)
            .map(Entry::deadline)
            .min()
    }

    /// Whether this state belongs to the facts' binding. A state for
    /// another session must be replaced, never consulted.
    pub fn bound(&self, facts: &Facts<'_>) -> bool {
        facts.binding == Some(&self.binding)
    }

    /// Every entry, with what [`qualifies`] says of it by the latest
    /// listing.
    pub fn standing<'s>(&'s self, facts: &Facts<'_>) -> Vec<(&'s Entry, Qualifies)> {
        self.attendances
            .iter()
            .map(|e| (e, self.qualifies(e, facts)))
            .collect()
    }

    fn qualifies(&self, entry: &Entry, facts: &Facts<'_>) -> Qualifies {
        match self.tool {
            Some(tool) => qualifies(entry, tool, self.snapshot.as_ref(), facts),
            None if entry.resolved.is_none() => Qualifies::No(Unqualified::NotResolved),
            None => Qualifies::No(Unqualified::Uncorrelated),
        }
    }

    pub fn step(&mut self, event: Event, facts: &Facts<'_>) -> Output {
        self.prune_received(facts);
        self.forget_stale_repeat(facts);
        let out = match event {
            Event::TurnEnd(report) => Output::Verdict(self.turn_end(report, facts)),
            Event::Attend(req) => Output::Attend(self.attend(req, facts)),
            Event::Delivered(id) => Output::Result(self.settle(id, Resolution::Delivered, facts)),
            Event::EmitFailed(id) => Output::Result(self.settle(id, Resolution::Failed, facts)),
            Event::CheckInDue => Output::Effects(self.check_in_due(facts)),
            Event::CheckInUndeliverable { check_ins, why } => {
                self.check_in_undeliverable(&check_ins, why);
                Output::Effects(Vec::new())
            }
            Event::Parked(report) => Output::Verdict(self.parked(report, facts)),
            Event::SessionStart(session) => Output::Verdict(self.session_start(session, facts)),
            Event::Observed => Output::Effects(Vec::new()),
        };
        self.prune_received(facts);
        self.forget_stale_repeat(facts);
        out
    }

    /// Evidence for work no longer owed can quiet nothing.
    fn prune_received(&mut self, facts: &Facts<'_>) {
        if !self.bound(facts) {
            return;
        }
        let owed = owed_identities(facts.owed);
        self.received.retain(|identity| owed.contains(identity));
    }

    /// The owed work any presentation may carry now. While any entry
    /// qualifies that is what this binding has not received; otherwise it
    /// is everything owed. Every presenting path and the repeat episode
    /// read this and nothing else, so quieted work is never presented and
    /// never counted.
    fn deliverable(&self, facts: &Facts<'_>) -> Vec<OwedItem> {
        let quieting = self.quieting(facts);
        facts
            .owed
            .iter()
            .filter(|o| !quieting || self.received.binary_search(&o.identity).is_err())
            .cloned()
            .collect()
    }

    /// The receipt mask is global: one qualifying entry quiets all received
    /// work. Which live tasks are covered is a separate, per-task question
    /// ([`Self::uncovered`]).
    fn quieting(&self, facts: &Facts<'_>) -> bool {
        self.attendances
            .iter()
            .any(|e| self.qualifies(e, facts) == Qualifies::Yes)
    }

    /// Every removal of entries goes through here, and says how many it
    /// removed. When the last entry holding resolved work leaves, what the
    /// set quieted is released: that work is presented again before any
    /// later entry can quiet it.
    fn retain_entries(&mut self, keep: impl FnMut(&Entry) -> bool) -> usize {
        let had_work = self.attendances.iter().any(|e| e.resolved.is_some());
        let before = self.attendances.len();
        self.attendances.retain(keep);
        if had_work && !self.attendances.iter().any(|e| e.resolved.is_some()) {
            self.received.clear();
        }
        before - self.attendances.len()
    }

    /// Entries whose task a known listing no longer has: that work is
    /// over, and the provider's completion wake reports it.
    fn end_finished(&mut self, facts: &Facts<'_>) {
        let Some(tool) = self.tool else {
            return;
        };
        let snapshot = self.snapshot.clone();
        self.retain_entries(|e| qualifies(e, tool, snapshot.as_ref(), facts) != Qualifies::Ended);
    }

    /// A run that never reached a listing and whose process has gone can
    /// never resolve: its work is over. Keeps the set bounded by live work
    /// where no listing ever arrives to resolve or end it.
    fn forget_gone_requests(&mut self, facts: &Facts<'_>) {
        self.retain_entries(|e| {
            e.resolved.is_some() || e.shell.as_ref().is_none_or(|p| facts.liveness.holds(p))
        });
    }

    fn arm_qualified(&mut self, facts: &Facts<'_>) {
        let Some(tool) = self.tool else {
            return;
        };
        for entry in &mut self.attendances {
            if entry.check_in == CheckIn::Unarmed
                && qualifies(entry, tool, self.snapshot.as_ref(), facts) == Qualifies::Yes
            {
                entry.check_in = CheckIn::Armed;
            }
        }
    }

    /// Today's `forget_if_stale`: a count describes one unbroken run of the
    /// same deliverable items, so any observed change, including nothing
    /// deliverable, ends it. Every step's facts are read when it is
    /// evaluated, so a result arriving late is judged by the work owed when
    /// it arrives.
    fn forget_stale_repeat(&mut self, facts: &Facts<'_>) {
        if !self.bound(facts) {
            return;
        }
        let owed = self.episode(facts);
        if self
            .outstanding
            .as_ref()
            .is_some_and(|p| p.identities != owed)
        {
            self.outstanding_episode_broken = true;
        }
        if self.repeat.as_ref().is_some_and(|r| r.items != owed) {
            self.repeat = None;
        }
    }

    /// What one repeat episode is made of: the deliverable owed work and
    /// the live work no entry covers. A presentation is counted against
    /// both, and a change in either ends the episode.
    fn episode(&self, facts: &Facts<'_>) -> Vec<String> {
        let mut items = owed_identities(&self.deliverable(facts));
        items.extend(self.uncovered(facts).iter().map(Uncovered::identity));
        items.sort();
        items.dedup();
        items
    }

    fn turn_end(&mut self, report: TurnEndReport, facts: &Facts<'_>) -> Verdict {
        if facts.binding != Some(&report.session) || !self.bound(facts) {
            return Verdict::Silent(SilentWhy::StaleBinding);
        }
        let notices = self.resolve_requests(&report.tasks);
        self.tool = Some(report.tool);
        self.snapshot = Some(report.tasks);
        self.end_finished(facts);
        self.forget_gone_requests(facts);

        if let Some(gated) = drive_gate(facts) {
            return gated;
        }
        self.arm_qualified(facts);

        if report.tool == Tool::Grok {
            return grok_verdict();
        }
        // Gated as every wake clank makes, and by a person's command
        // waiting for the prompt, which a demand must not pre-empt.
        let demands = if wake_gate(facts).is_ok() && !facts.user_command {
            self.uncovered(facts)
        } else {
            Vec::new()
        };
        release_for_user_command(
            self.deliver_or_park(facts, Some(&report.place), notices, demands),
            facts,
        )
    }

    /// The live tasks the latest listing shows that no qualifying entry
    /// covers, limited to work an allowed command could cover: a subagent
    /// whose name alone resolves to it, or a shell with a task id, in a
    /// known listing, for a tool whose completion wake makes attending
    /// meaningful. A demand the agent could not satisfy would wake it
    /// forever: so no nameless or name-sharing subagent, and never clank's
    /// own `clank wait`.
    fn uncovered(&self, facts: &Facts<'_>) -> Vec<Uncovered> {
        if !self.tool.is_some_and(completion_wake) {
            return Vec::new();
        }
        let Some(live) = self.snapshot.as_ref().and_then(TaskSnapshot::live) else {
            return Vec::new();
        };
        let live: Vec<&BackgroundTask> = live.collect();
        live.iter()
            .filter_map(|&task| {
                let task_id = task.id.clone()?;
                let work = match TaskKind::of(task) {
                    TaskKind::Subagent => {
                        let name = task.description.clone()?;
                        let resolves = !name.is_empty()
                            && name.trim() == name
                            && resolve(&Selector::Name(name.clone()), &live)
                                .is_ok_and(|only| only.id == task.id);
                        resolves.then_some(Attendable::Subagent { name })?
                    }
                    TaskKind::Shell if !task.command.as_deref().is_some_and(clank_wait) => {
                        Attendable::Shell {
                            run: task.command.as_deref().and_then(clank_run_desc),
                        }
                    }
                    TaskKind::Shell | TaskKind::Unknown(_) => return None,
                };
                let verdicts: Vec<Qualifies> = self
                    .attendances
                    .iter()
                    .filter(|e| e.task() == Some(task_id.as_str()))
                    .map(|e| self.qualifies(e, facts))
                    .collect();
                if verdicts.contains(&Qualifies::Yes) {
                    return None;
                }
                Some(Uncovered {
                    entry: verdicts.into_iter().find_map(|q| match q {
                        Qualifies::No(why) => Some(why),
                        Qualifies::Yes | Qualifies::Ended => None,
                    }),
                    task_id,
                    work,
                })
            })
            .collect()
    }

    fn parked(&mut self, report: TurnEndReport, facts: &Facts<'_>) -> Verdict {
        if facts.binding != Some(&report.session) || !self.bound(facts) {
            return Verdict::Silent(SilentWhy::StaleBinding);
        }
        if let Some(gated) = drive_gate(facts) {
            return gated;
        }
        if report.tool == Tool::Grok {
            return grok_verdict();
        }
        release_for_user_command(
            self.deliver_or_park(facts, Some(&report.place), Vec::new(), Vec::new()),
            facts,
        )
    }

    fn session_start(&mut self, session: SessionId, facts: &Facts<'_>) -> Verdict {
        if facts.binding != Some(&session) || !self.bound(facts) {
            return Verdict::Silent(SilentWhy::StaleBinding);
        }
        if let Some(gated) = drive_gate(facts) {
            return gated;
        }
        self.deliver_or_park(facts, None, Vec::new(), Vec::new())
    }

    /// The [`deliverable`](Self::deliverable) work and the uncovered
    /// work `demands` names as one new presentation, or Park when there is
    /// neither or another presentation is in flight. A park keeps the
    /// hook's wait armed, so work that arrives later still wakes the agent.
    /// `place` is the Stop it counts against; without one it is not
    /// counted.
    fn deliver_or_park(
        &mut self,
        facts: &Facts<'_>,
        place: Option<&TurnPlace>,
        mut notices: Vec<Notice>,
        demands: Vec<Uncovered>,
    ) -> Verdict {
        let deliverable = self.deliverable(facts);
        let demanded: Vec<String> = demands.iter().map(Uncovered::identity).collect();
        if !demands.is_empty() {
            notices.push(Notice::Uncovered(demands));
        }
        if (deliverable.is_empty() && demanded.is_empty()) || self.outstanding.is_some() {
            return if notices.is_empty() {
                Verdict::Park
            } else {
                Verdict::Notify(notices)
            };
        }
        let p = self.present(&deliverable, &demanded, place);
        let repeat = p.repeat.count();
        let verdict = Verdict::Deliver {
            presentation: p.id.clone(),
            identities: p.identities.clone(),
            repeat,
            escalated: repeat >= ESCALATE_AT,
            notices,
        };
        self.outstanding = Some(p);
        verdict
    }

    /// Resolve every pending entry against THIS report's listing, which is
    /// the first that can contain work started in the turn that asked for
    /// it. Each resolves, or is dropped with its own notice, alone: no
    /// other entry is touched. One that resolves to a task an entry already
    /// covers renews that entry instead of doubling it.
    fn resolve_requests(&mut self, tasks: &TaskSnapshot) -> Vec<Notice> {
        let Some(live) = tasks.live() else {
            return Vec::new();
        };
        let live: Vec<&BackgroundTask> = live.collect();
        let mut notices = Vec::new();
        let (mut kept, pending): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut self.attendances)
            .into_iter()
            .partition(|e| e.resolved.is_some());
        for mut entry in pending {
            if let Some(name) = run_alias(&entry.selector, &live) {
                let runs: Vec<&mut Entry> = kept
                    .iter_mut()
                    .filter(|e| {
                        e.is_run()
                            && e.selector.text() == name
                            && e.task()
                                .is_some_and(|id| live.iter().any(|t| t.id.as_deref() == Some(id)))
                    })
                    .collect();
                if !runs.is_empty() {
                    for run in runs {
                        run.renew(entry.since, entry.expect, None);
                    }
                    continue;
                }
            }
            let task = match resolve(&entry.selector, &live) {
                Ok(task) => task,
                Err(notice) => {
                    notices.push(notice);
                    continue;
                }
            };
            let kind = TaskKind::of(task);
            let task_id = task.id.clone().unwrap_or_default();
            if let Selector::Name(name) = &entry.selector
                && kind == TaskKind::Shell
            {
                notices.push(Notice::ShellNamed {
                    name: name.clone(),
                    task_id,
                });
                continue;
            }
            if let Some(held) = kept.iter_mut().find(|e| e.task() == Some(task_id.as_str())) {
                held.renew(entry.since, entry.expect, entry.desc);
                continue;
            }
            entry.desc = entry.desc.or_else(|| task.description.clone());
            entry.resolved = Some(Resolved { task_id, kind });
            entry.check_in = CheckIn::Unarmed;
            kept.push(entry);
        }
        self.attendances = kept;
        notices
    }

    /// Always a new presentation, counted from the last COMMITTED repeat
    /// record. A failed one is never reused: its count belongs to the
    /// episode it was made in, which an observed change may since have
    /// ended.
    fn present(
        &mut self,
        owed: &[OwedItem],
        demanded: &[String],
        place: Option<&TurnPlace>,
    ) -> Presentation {
        let mut identities = owed_identities(owed);
        identities.extend_from_slice(demanded);
        identities.sort();
        identities.dedup();
        let receipts: Vec<String> = owed.iter().flat_map(|o| o.receipts.clone()).collect();
        self.next_seq += 1;
        Presentation {
            id: PresentationId {
                session: self.binding.clone(),
                seq: self.next_seq,
            },
            repeat: match place {
                Some(place) => count_delivery(self.repeat.as_ref(), &identities, place),
                None => RepeatDecision::Uncounted,
            },
            identities,
            receipts,
        }
    }

    fn attend(&mut self, req: AttendRequest, facts: &Facts<'_>) -> AttendReply {
        let session = match &req {
            AttendRequest::Attend { session, .. } | AttendRequest::Clear { session, .. } => session,
        };
        if facts.binding != Some(session) || !self.bound(facts) {
            return AttendReply::RefusedStale;
        }
        match req {
            AttendRequest::Attend {
                selector,
                desc,
                expect,
                shell,
                ..
            } => self.add(selector, desc, expect, shell, facts),
            AttendRequest::Clear { which, .. } => self.clear(which),
        }
    }

    /// A request adds an entry, or renews the one already attending the
    /// same work. It never removes or renews any other.
    fn add(
        &mut self,
        selector: Selector,
        desc: Option<String>,
        expect: Ms,
        shell: Option<ShellProof>,
        facts: &Facts<'_>,
    ) -> AttendReply {
        self.forget_gone_requests(facts);
        let renewing: Vec<usize> = match (&selector, &shell) {
            (Selector::RunDesc(_), Some(proof)) => {
                match self
                    .attendances
                    .iter()
                    .position(|e| e.shell.as_ref() == Some(proof))
                {
                    Some(same) => vec![same],
                    None if self
                        .attendances
                        .iter()
                        .any(|e| e.is_run() && e.selector.text() == selector.text()) =>
                    {
                        return AttendReply::DescInUse(selector.text().to_string());
                    }
                    None => Vec::new(),
                }
            }
            // Only a request for this very name still waiting for a listing.
            // A resolved entry under the name may be a task that has since
            // ended and been replaced: which task the name means now, and so
            // whether this renews that entry, is the listing's to show
            // ([`Self::resolve_requests`]).
            (Selector::Name(_), _) => self
                .attendances
                .iter()
                .enumerate()
                .filter(|(_, e)| e.resolved.is_none() && e.selector == selector)
                .map(|(i, _)| i)
                .collect(),
            (Selector::RunDesc(_) | Selector::TaskId(_), _) => Vec::new(),
        };
        if renewing.is_empty() {
            self.attendances.push(Entry {
                selector,
                resolved: None,
                desc,
                since: facts.now,
                expect,
                shell,
                check_in: CheckIn::Unarmed,
            });
            return AttendReply::Accepted;
        }
        for i in renewing {
            self.attendances[i].renew(facts.now, expect, desc.clone());
        }
        AttendReply::Renewed
    }

    fn clear(&mut self, which: Clearing) -> AttendReply {
        let removed = match which {
            Clearing::All => self.retain_entries(|_| false),
            Clearing::Run(proof) => self.retain_entries(|e| e.shell.as_ref() != Some(&proof)),
            Clearing::Named(name) => {
                let named: Vec<&Entry> = self
                    .attendances
                    .iter()
                    .filter(|e| e.selector.text() == name)
                    .collect();
                if named.iter().any(|e| e.is_run()) && named.iter().any(|e| !e.is_run()) {
                    return AttendReply::Ambiguous(
                        named.iter().map(|e| e.selector.clone()).collect(),
                    );
                }
                self.retain_entries(|e| e.selector.text() != name)
            }
        };
        AttendReply::Cleared { removed }
    }

    /// Settle the in-flight presentation with its one result. A success
    /// commits its receipts and its repeat decision, which was made
    /// against the record nothing could replace while it was in flight,
    /// and is the only evidence that its work was received. Only owed work
    /// is received: an uncovered task's demand is counted, never quieted.
    fn settle(&mut self, id: PresentationId, how: Resolution, facts: &Facts<'_>) -> ResultOutcome {
        if !self.bound(facts) {
            return ResultOutcome::ProtocolViolation(id);
        }
        let Some(p) = self.outstanding.take_if(|p| p.id == id) else {
            return if self.resolved == Some((id.clone(), how)) {
                ResultOutcome::Duplicate
            } else {
                ResultOutcome::ProtocolViolation(id)
            };
        };
        self.resolved = Some((id, how));
        let broken = std::mem::take(&mut self.outstanding_episode_broken);
        if how == Resolution::Failed {
            return ResultOutcome::Resolved(Vec::new());
        }
        let owed = owed_identities(facts.owed);
        for identity in p.identities.into_iter().filter(|i| owed.contains(i)) {
            if let Err(at) = self.received.binary_search(&identity) {
                self.received.insert(at, identity);
            }
        }
        let mut effects = vec![Effect::CommitReceipts(p.receipts)];
        if let RepeatDecision::Counted(next) = p.repeat {
            if !broken {
                self.repeat = Some(next);
            }
            self.forget_stale_repeat(facts);
            effects.push(Effect::CommitRepeat(self.repeat.clone()));
        }
        ResultOutcome::Resolved(effects)
    }

    /// Every entry whose check-in falls due, in ONE check-in. Not gated on
    /// [`qualifies`]: at the deadline it is false by definition, and the
    /// check-in is exactly the wake that deadline owes. It is gated on
    /// [`wake_gate`], as every wake clank makes is, and each due entry
    /// resolves either way, so no due timer is scheduled again. An entry
    /// not yet due is not touched.
    fn check_in_due(&mut self, facts: &Facts<'_>) -> Vec<Effect> {
        if !self.bound(facts) {
            return Vec::new();
        }
        let gate = wake_gate(facts);
        let mut lines = Vec::new();
        for entry in &mut self.attendances {
            if entry.check_in != CheckIn::Armed || facts.now < entry.deadline() {
                continue;
            }
            if entry
                .shell
                .as_ref()
                .is_some_and(|p| !facts.liveness.holds(p))
            {
                entry.check_in = CheckIn::ProcessGone;
                continue;
            }
            if let Err(why) = gate {
                entry.check_in = CheckIn::Withheld(why);
                continue;
            }
            if let Some(line) = check_in_line(entry, facts.now) {
                entry.check_in = CheckIn::Fired;
                lines.push(line);
            }
        }
        if lines.is_empty() {
            Vec::new()
        } else {
            vec![Effect::CheckIn(lines)]
        }
    }

    /// Only an entry still holding the check-in fired for it records its
    /// loss: a renewal made while it was being typed has its own.
    fn check_in_undeliverable(&mut self, fired: &[CheckInRef], why: String) {
        for entry in &mut self.attendances {
            if fired.iter().any(|r| is_fired(entry, r)) {
                entry.check_in = CheckIn::Undeliverable(why.clone());
            }
        }
    }

    /// The last look before a fired check-in is typed: the session still
    /// bound and every wake gate still open. What may be typed is only the
    /// entries it was fired for that still hold it, as they stand now: one
    /// renewed or cleared while it was prepared drops out, and the rest
    /// are still owed theirs.
    pub fn may_type_check_in(
        &self,
        fired: &[CheckInRef],
        facts: &Facts<'_>,
    ) -> Result<Vec<CheckInLine>, String> {
        if !self.bound(facts) {
            return Err("the session no longer holds the label".into());
        }
        wake_gate(facts).map_err(|why| {
            match why {
                Withheld::AutoOff => "auto turned off",
                Withheld::Asking => "a question is pending",
                Withheld::Blocked => "a block is pending",
            }
            .to_string()
        })?;
        let lines: Vec<CheckInLine> = self
            .attendances
            .iter()
            .filter(|e| fired.iter().any(|r| is_fired(e, r)))
            .filter_map(|e| check_in_line(e, facts.now))
            .collect();
        if lines.is_empty() {
            return Err("the attendance it was for was cleared or renewed".into());
        }
        Ok(lines)
    }

    /// Why recovery must leave this agent alone, if it must. Recovery has
    /// no rule of its own: attendance holds it only while it quiets every
    /// owed item, by the same deliverable projection a turn-end presents
    /// from, so work the session never received stays recoverable.
    /// `facts.owed` must be the work actually owed: none owed is quiet.
    pub fn recovery_quiet(&self, facts: &Facts<'_>) -> Option<Quiet> {
        if !self.bound(facts) {
            return None;
        }
        if facts.blocked {
            return Some(Quiet::Blocked);
        }
        if facts.asking {
            return Some(Quiet::Asking);
        }
        (self.quieting(facts) && self.deliverable(facts).is_empty()).then_some(Quiet::Attending)
    }
}

/// `entry`'s line in a check-in at `now`; `None` before it has resolved.
fn check_in_line(entry: &Entry, now: Ms) -> Option<CheckInLine> {
    let Resolved { task_id, kind } = entry.resolved.as_ref()?;
    Some(CheckInLine {
        check_in: CheckInRef {
            key: entry.key(),
            since: entry.since,
        },
        task_id: task_id.clone(),
        kind: kind.clone(),
        desc: entry.desc.clone(),
        selector: entry.selector.clone(),
        elapsed: now.saturating_sub(entry.since),
        expect: entry.expect,
    })
}

/// Whether `entry` is still the one `fired` was fired for, with that
/// check-in still in flight.
fn is_fired(entry: &Entry, fired: &CheckInRef) -> bool {
    entry.check_in == CheckIn::Fired && entry.since == fired.since && entry.key() == fired.key
}

/// The `--desc` a name request may mean instead, when no live subagent
/// carries that name: a run is renewed by its own `--desc`, or past its
/// expectation it could never be given longer. Only a run entry this
/// listing proves still holds its task may take it.
fn run_alias<'s>(selector: &'s Selector, live: &[&BackgroundTask]) -> Option<&'s str> {
    let Selector::Name(name) = selector else {
        return None;
    };
    (!live
        .iter()
        .any(|t| TaskKind::of(t) == TaskKind::Subagent && selector.matches(t)))
    .then_some(name.as_str())
}

/// The one live task `selector` names, or the notice saying why there is
/// none.
fn resolve<'t>(
    selector: &Selector,
    live: &[&'t BackgroundTask],
) -> Result<&'t BackgroundTask, Notice> {
    let matches: Vec<&BackgroundTask> = live
        .iter()
        .copied()
        .filter(|t| selector.matches(t))
        .collect();
    match matches.as_slice() {
        [only] if only.id.is_some() => Ok(only),
        [] | [_] => Err(Notice::NoMatch {
            selector: selector.clone(),
            live: live.iter().copied().map(TaskSummary::from).collect(),
        }),
        many => Err(Notice::Ambiguous {
            selector: selector.clone(),
            matches: many.iter().copied().map(TaskSummary::from).collect(),
        }),
    }
}

/// The gates every presentation passes before attendance or work: an
/// unreadable config, auto off, then an unresolvable role.
fn drive_gate(facts: &Facts<'_>) -> Option<Verdict> {
    match &facts.auto {
        Err(e) => return Some(Verdict::Failed(e.clone())),
        Ok(AutoMode::Off) => return Some(Verdict::Silent(SilentWhy::AutoOff)),
        Ok(AutoMode::On) => {}
    }
    facts
        .role
        .as_ref()
        .err()
        .map(|e| Verdict::Failed(e.clone()))
}

/// A Stop that would park ends instead while a person's command waits:
/// the command needs an idle prompt, and a held hook is not one. Work to
/// deliver still wins, since the command waits for that turn's end too.
fn release_for_user_command(verdict: Verdict, facts: &Facts<'_>) -> Verdict {
    match verdict {
        Verdict::Park if facts.user_command => Verdict::Silent(SilentWhy::UserCommand),
        other => other,
    }
}

fn grok_verdict() -> Verdict {
    Verdict::Failed("hook: Grok native handoffs do not use the in-hook wait route".into())
}

fn owed_identities(owed: &[OwedItem]) -> Vec<String> {
    let mut identities: Vec<String> = owed.iter().map(|o| o.identity.clone()).collect();
    identities.sort();
    identities.dedup();
    identities
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quiet {
    Blocked,
    Asking,
    Attending,
}

/// What a hook does when no monitor can decide: the generic owed-work fold
/// only says whether work is owed. Never quiet on attendance, never a
/// presentation, so the monitor keeps custody of every receipt.
/// What Grok's native Stop showed: whether a `clank wait` was already
/// running, and whether this Stop is the continuation of one that asked
/// for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    pub session: SessionId,
    pub wait_running: bool,
    pub continued: bool,
}

/// Grok's native turn-end, with or without a monitor: the drive gate, then
/// a request for the background wait unless one already carries the wake.
/// A continuation never asks again, so a wait that failed is not
/// requested in a loop within one turn.
pub fn handoff_verdict(handoff: &Handoff, facts: &Facts<'_>) -> Verdict {
    if facts.binding != Some(&handoff.session) {
        return Verdict::Silent(SilentWhy::StaleBinding);
    }
    if let Some(gated) = drive_gate(facts) {
        return gated;
    }
    if handoff.wait_running || handoff.continued {
        return Verdict::Silent(SilentWhy::WaitArmed);
    }
    Verdict::Notify(vec![Notice::ArmWait])
}

pub fn alone_verdict(session: &SessionId, facts: &Facts<'_>) -> Verdict {
    if facts.binding != Some(session) {
        return Verdict::Silent(SilentWhy::StaleBinding);
    }
    if let Some(gated) = drive_gate(facts) {
        return gated;
    }
    if facts.asking || facts.blocked || facts.owed.is_empty() {
        return Verdict::Park;
    }
    Verdict::Notify(vec![Notice::MonitorUnavailable])
}

#[cfg(test)]
mod tests;
