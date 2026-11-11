//! The turn-end policy: one pure state machine per (repo, agent, session
//! binding) that decides what every Stop, attendance request, check-in,
//! delivery result and recovery question means.
//!
//! The provider hooks report evidence and render [`Verdict`]s; they decide
//! nothing (the-stop-hook-is-a-messenger). Every transition is a function of
//! the state, the event and [`Facts`] the monitor gathers when it evaluates.
//! Facts are never cached: a proof is re-checked each time it is used.
//!
//! clank never detects background work. What an agent attends is only what
//! it declared: a `clank run` (its registered process) or a `clank attend`
//! declaration. A harness's own task listing is read only to resolve and
//! end a declaration, which a subagent's work may outlast
//! ([`transition`]); a listed task nobody declared is ignored.
//!
use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;

use crate::hook_io::{BackgroundTask, HookInput};
use crate::ids::SessionId;
use crate::vocab::{AutoMode, Role, Tool};

mod child;

pub use child::{HeldTask, Order, Phase, Sight, Tracked, transition};

/// Milliseconds since the Unix epoch.
pub type Ms = u64;

/// What the harness said of its own background work at one Stop.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "snapshot", content = "tasks", rename_all = "snake_case")]
pub enum TaskSnapshot {
    /// The harness sent a listing, possibly empty: a task missing from it
    /// is not running.
    Known(Vec<BackgroundTask>),
    /// No listing (codex, OpenCode, a payload that omitted it). Evidence of
    /// neither completion nor liveness.
    #[default]
    Unavailable,
}

impl TaskSnapshot {
    /// The snapshot a Stop's payload carries: a list when the harness sent
    /// one, however empty, and `Unavailable` when it omitted it.
    pub fn of(input: &HookInput) -> Self {
        Self::listed(input.background_tasks.clone())
    }

    pub fn listed(tasks: Option<Vec<BackgroundTask>>) -> Self {
        match tasks {
            Some(tasks) => Self::Known(tasks),
            None => Self::Unavailable,
        }
    }

    fn live(&self) -> Option<Vec<&BackgroundTask>> {
        match self {
            Self::Known(tasks) => Some(tasks.iter().filter(|t| t.live()).collect()),
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
    /// How much of the record the adapter had in front of it when it read
    /// this place. `None` where it read no record, and from a hook built
    /// before it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_to: Option<ReadTo>,
}

/// A record as an adapter saw it: which file, and its length then. Only
/// ever compared with the same two measures of the same record, never
/// with a clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadTo {
    pub file: u64,
    pub len: u64,
}

/// A Stop the agent's hook reported: a turn of the bound session was over
/// when the monitor was told, whatever the harness's record says of it
/// while its hook runs. Which turn is only as known as the adapter could
/// place it, and a reader may claim no more than that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stopped {
    /// The turn that ended, where the adapter names it.
    pub turn: Option<String>,
    /// That turn's newest boundary in the record as the hook read it. With
    /// it the Stop speaks for `turn` exactly while the boundary has not
    /// moved.
    pub position: Option<u64>,
    /// The record as the hook saw it. A Stop with no boundary speaks for
    /// the record's latest turn only while the record is still exactly
    /// this; without it such a Stop ends no turn. Absent from a Stop kept
    /// before hooks said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_to: Option<ReadTo>,
    /// When the monitor was first told: after the hook read the record,
    /// by however long the report took to arrive. Never evidence that the
    /// record was unwritten at the Stop.
    pub at: Ms,
    /// When the Stop's hook started, by the hook's own clock. Absent from
    /// a hook that did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_started: Option<Ms>,
    /// The ids the Stop's listing held. `None` where it carried none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listed: Option<Vec<String>>,
}

impl Stopped {
    /// Whether `record`, the agent's record as a later hook of its session
    /// read it, is still what this Stop's hook saw: the same boundary of
    /// the same turn, or with no boundary the same file at the same
    /// length. Only then did no turn of the agent's begin in between.
    pub fn stands_in(&self, record: &TurnPlace) -> bool {
        match self.position {
            Some(position) => {
                self.turn.is_some() && record.turn == self.turn && record.position == Some(position)
            }
            None => self.read_to.is_some() && record.read_to == self.read_to,
        }
    }
}

/// One Stop, as the hook observed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEndReport {
    pub tool: Tool,
    pub session: SessionId,
    pub place: TurnPlace,
    /// A hook from before listings were reported again sends none.
    #[serde(default)]
    pub tasks: TaskSnapshot,
    /// When the hook started, read by the hook from the host clock: the
    /// only evidence of where this report sits among its session's
    /// others. A hook from before it was reported sends none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_started: Option<Ms>,
}

/// What a subagent's own hook reported: it started, or its turn ended.
/// Evidence about a declaration and nothing else: it ends no turn of the
/// agent's, parks nothing and is owed no verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildReport {
    pub session: SessionId,
    /// The harness's id for the subagent, which is its task's id in a
    /// listing.
    pub child: String,
    /// As [`TurnEndReport::hook_started`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_started: Option<Ms>,
    pub seen: ChildSeen,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildSeen {
    /// Spawned or resumed. The harness says which of neither, and sends
    /// no listing.
    Started,
    /// Its turn ended, to wait on work of its own or for good.
    Stopped {
        /// The session's listing then. It still shows the subagent
        /// itself as running.
        tasks: TaskSnapshot,
        /// The agent's own record as this hook read it.
        agent: TurnPlace,
    },
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

/// A `clank run`'s process, as it registered itself: a pid alone is
/// reused, so the birth token says which process it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellProof {
    pub pid: i32,
    pub token: String,
}

/// What the system can say of a recorded process now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Positively still that process.
    Live,
    /// Positively not: no such process, or its pid carries another birth.
    Gone,
    /// Neither can be established (an unreadable or unrecognised identity,
    /// a denied or failed read). Never evidence that it ended.
    Unknown,
}

/// Whether a recorded process is still that process. The monitor answers
/// it at evaluation time; nothing caches the answer.
pub trait Liveness {
    fn presence(&self, proof: &ShellProof) -> Presence;
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

/// An entry's one check-in per expectation, armed from the moment the
/// entry is accepted or renewed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "legacy::StoredCheckIn")]
pub enum CheckIn {
    Armed,
    /// Its expectation has passed and no presentation carrying its
    /// check-in has been delivered yet: the agent's next park is owed it.
    Due,
    /// A presentation carrying it was delivered.
    Fired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Withheld {
    AutoOff,
    Asking,
    Blocked,
}

/// Which expectation window a check-in was presented for. A renewal opens
/// a new window, so a result for the old one is told apart; binding or
/// merging a declaration moves its window, so a result still finds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "legacy::StoredRef")]
pub struct CheckInRef {
    pub window: WindowId,
}

/// A window's identity, unique within its binding. Zero names no window.
pub type WindowId = u64;

/// One expectation of an entry's: from `since`, for `expect`, with its one
/// check-in. Created only by a declaration, a run or a renewal; binding
/// and merging move it whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    /// Absent from state saved before windows had identities, which loading
    /// assigns.
    #[serde(rename = "window", default)]
    pub id: WindowId,
    pub since: Ms,
    pub expect: Ms,
    pub check_in: CheckIn,
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

/// What one attended piece of work is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Attending {
    /// A `clank run`: its registered process is the work, so the work
    /// ends exactly when that process is gone.
    Run(ShellProof),
    /// A `clank attend` declaration. Where the harness lists its tasks it
    /// resolves to the one live task whose description is `name`, and ends
    /// by [`transition`]: when that task has gone and so has any work it
    /// may have started. Without a listing only the agent's clear ends it;
    /// its deadline is a check-in, never proof that the work finished.
    Declared {
        name: String,
        /// The harness's stable id of the task it resolved to. Bound once:
        /// a later task reusing the description is not this one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
        /// The listed kind of `task` when it bound (`subagent`, `shell`,
        /// …; empty when the listing gave none), which decides how it ends.
        /// Absent only from a record bound before kinds were kept.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        /// The agent's Stop listings in a row that had no task by this
        /// name.
        #[serde(default, skip_serializing_if = "is_zero")]
        misses: u8,
        /// What the session's reports have shown of `task` since it was
        /// bound. State saved before it was kept reads as a task running
        /// with no baseline.
        #[serde(default, skip_serializing_if = "Tracked::is_unobserved")]
        tracked: Tracked,
    },
}

fn is_zero(n: &u8) -> bool {
    *n == 0
}

/// Known listings a declaration may go unmatched before it is dropped:
/// the work may be started in the turn after the one that declared it.
pub const MISSES_ALLOWED: u8 = 2;

impl Attending {
    pub fn declared(name: String) -> Self {
        Self::Declared {
            name,
            task: None,
            kind: None,
            misses: 0,
            tracked: Tracked::default(),
        }
    }
}

/// What every mutation of an entry is scoped to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKey {
    Name(String),
    /// A declaration resolved to this listed task: a renewal of the same
    /// name may stand beside it until a listing resolves that too.
    Task(String),
    Run(ShellProof),
}

/// One attended piece of background work, with its own expectation and
/// check-in. Its clock runs from `since`, the moment it was accepted or
/// last renewed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub work: Attending,
    /// What status shows, and the name `clank attend` renews or clears a
    /// run by.
    pub desc: String,
    /// When it was first accepted; renewals leave it.
    pub started: Ms,
    #[serde(flatten)]
    pub window: Window,
}

impl Entry {
    pub fn key(&self) -> EntryKey {
        match &self.work {
            Attending::Run(proof) => EntryKey::Run(proof.clone()),
            Attending::Declared {
                task: Some(task), ..
            } => EntryKey::Task(task.clone()),
            Attending::Declared { name, .. } => EntryKey::Name(name.clone()),
        }
    }

    /// The listed task this declaration resolved to.
    pub fn task(&self) -> Option<&str> {
        match &self.work {
            Attending::Declared { task, .. } => task.as_deref(),
            Attending::Run(_) => None,
        }
    }

    /// Where a declaration's task stands by its session's reports.
    pub fn phase(&self) -> Option<&Phase> {
        match &self.work {
            Attending::Declared { tracked, .. } => Some(&tracked.phase),
            Attending::Run(_) => None,
        }
    }

    /// The name `clank attend` renews or clears this entry by.
    pub fn name(&self) -> &str {
        match &self.work {
            Attending::Run(_) => &self.desc,
            Attending::Declared { name, .. } => name,
        }
    }

    pub fn proof(&self) -> Option<&ShellProof> {
        match &self.work {
            Attending::Run(proof) => Some(proof),
            Attending::Declared { .. } => None,
        }
    }

    pub fn deadline(&self) -> Ms {
        self.window.since.saturating_add(self.window.expect)
    }

    /// A new expectation, in its own window.
    fn renew(&mut self, window: Window, desc: Option<String>) {
        self.window = window;
        if let Some(desc) = desc {
            self.desc = desc;
        }
    }
}

/// Why an entry does or does not quiet anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Qualifies {
    Yes,
    No(Unqualified),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unqualified {
    /// A run whose registered process is gone: it has ended, and its
    /// terminal record is being written.
    ProcessGone,
    /// A run whose process cannot be seen now: still registered, but
    /// nothing proves the work runs.
    ProcessUnknown,
    PastDeadline,
}

/// THE rule. Turn-end and recovery ask this and nothing else whether an
/// entry may quiet anything.
pub fn qualifies(entry: &Entry, facts: &Facts<'_>) -> Qualifies {
    match entry.proof().map(|p| facts.liveness.presence(p)) {
        Some(Presence::Gone) => return Qualifies::No(Unqualified::ProcessGone),
        Some(Presence::Unknown) => return Qualifies::No(Unqualified::ProcessUnknown),
        Some(Presence::Live) | None => {}
    }
    if facts.now >= entry.deadline() {
        return Qualifies::No(Unqualified::PastDeadline);
    }
    Qualifies::Yes
}

/// A run whose registered process is gone: everything its one terminal
/// record says. clank captured no output and claims no exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEnd {
    pub proof: ShellProof,
    pub desc: String,
    pub started: Ms,
    /// When clank saw the process gone, which is not when it exited.
    pub ended_seen: Ms,
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
    /// When it was handed to its carrier. Absent from state saved before
    /// a result had a bound; such a presentation died with its monitor.
    #[serde(default)]
    pub handed: Ms,
    /// The check-ins it carries, each spent only when it is delivered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub check_ins: Vec<CheckInRef>,
}

/// How long a carrier has to answer for the presentation it was handed.
///
/// A hook answers as soon as its output is flushed, so a minute is a
/// carrier that will not answer, not a slow one. Not the park ceiling:
/// that bounds a wait for work, and this bounds a write.
pub const DELIVERY_RESULT_WITHIN: Ms = 60_000;

impl Presentation {
    /// The moment its carrier's silence is a failed delivery.
    pub fn expires(&self) -> Ms {
        self.handed.saturating_add(DELIVERY_RESULT_WITHIN)
    }
}

/// Text a verdict carries that is not work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Notice {
    /// A declaration matched no listed task within [`MISSES_ALLOWED`]
    /// listings, so it was dropped. `live` is the latest listing.
    NoMatch {
        name: String,
        live: Vec<TaskSummary>,
    },
    /// A declaration matched one listed task that carries no id, so its end
    /// could never be told from another task's: it was dropped.
    Untrackable { name: String, task: TaskSummary },
    /// A declaration's name was the description of several listed tasks,
    /// so it was dropped rather than guess.
    Ambiguous {
        name: String,
        matches: Vec<TaskSummary>,
    },
    /// No monitor could be reached or started within the waiter's outage
    /// budget. `why` is how the last attempt ended.
    MonitorOutage { why: String },
    /// A monitor took a report that never parks and gave no verdict
    /// before its hook had to answer.
    MonitorUnanswered,
    /// Grok's native handoff: run `clank wait` in the background, whose
    /// completion wakes the agent.
    ArmWait,
    /// One check-in for every entry owed one when it was presented.
    CheckIn(Vec<CheckInLine>),
}

/// A pending declaration merging into the one already bound to `task`.
struct Merge {
    task: String,
    window: Window,
    desc: String,
}

/// A listed task, as a declaration notice names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: Option<String>,
    pub kind: Option<String>,
    pub description: Option<String>,
}

impl From<&BackgroundTask> for TaskSummary {
    fn from(t: &BackgroundTask) -> Self {
        Self {
            id: t.id.clone(),
            kind: t.kind.clone(),
            description: t.description.clone(),
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
    /// A `clank run` registering the process it is about to become.
    Run {
        session: SessionId,
        proof: ShellProof,
        desc: String,
        expect: Ms,
    },
    /// A `clank attend` declaration, or the renewal of whatever already
    /// stands under `name`.
    Declare {
        session: SessionId,
        name: String,
        desc: Option<String>,
        expect: Ms,
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
    /// Every entry standing under this name: a declaration's, or a run's
    /// `--desc`.
    Named(String),
    /// Exactly the run this process proof registered.
    Run(ShellProof),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttendReply {
    /// A new entry, in force now.
    Accepted,
    /// What already stood under this name or process now runs on the new
    /// expectation.
    Renewed,
    Cleared {
        removed: usize,
    },
    RefusedStale,
}

/// A side effect the monitor performs for a transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    CommitReceipts(Vec<String>),
    CommitRepeat(Option<RepeatRecord>),
}

/// One entry's line in a check-in, as it stood when presented.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckInLine {
    pub check_in: CheckInRef,
    pub desc: String,
    /// What `clank attend` renews it by.
    pub name: String,
    /// A `clank run`, whose end wakes the agent; otherwise a declaration,
    /// whose end nothing reports.
    pub run: bool,
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
    /// [`Event::ResultOverdue`] found nothing in flight past its bound.
    InTime,
    /// A result for a presentation that is not in flight, and that this
    /// result did not just settle. The transport makes this impossible,
    /// so it is refused and commits nothing.
    ProtocolViolation(PresentationId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A Stop: a turn of the agent's ended. A report answered Park is
    /// stepped again with the same report when the state or facts change.
    TurnEnd(TurnEndReport),
    Attend(AttendRequest),
    /// Only from the connection that carried the in-flight presentation.
    /// The agent starts a turn on it.
    Delivered(PresentationId),
    /// The emit failed, or the hook's connection closed without a result:
    /// the monitor turns a closed connection into this, so every
    /// presentation resolves. Terminal: no later success can exist.
    EmitFailed(PresentationId),
    /// The presentation in flight has had [`DELIVERY_RESULT_WITHIN`] and
    /// no result: it failed, terminally, as a closed connection fails it.
    /// A result that arrives afterwards finds nothing in flight.
    ResultOverdue,
    /// An entry's expectation may have passed. Every step makes each one
    /// passed owed its check-in; this one exists so the moment is stepped
    /// when nothing else happens.
    CheckInDue,
    /// The very Stop report a park holds, stepped again because the state
    /// or facts changed. It is not a new Stop: no turn ended. Only the
    /// carrier holding that report may send it.
    Parked(TurnEndReport),
    /// A compaction's report of the park the last Stop armed: a new report,
    /// stepped as that park, which holds the Stop only by its own placement
    /// ([`Stopped::stands_in`]).
    Compacted(TurnEndReport),
    /// A session started: its owed work, presented once and never counted
    /// as a repeat, or nothing.
    SessionStart(SessionId),
    /// Grok's native Stop: answered by [`handoff_verdict`], and its listing
    /// resolves and ends declarations as a Stop's does.
    NativeTurnEnd(Handoff),
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

/// What survives a monitor restart: every entry, a run's registration
/// included, so a run that ended while no monitor watched is still
/// recorded once.
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
    /// The last window identity this binding gave out.
    pub next_window: WindowId,
    /// State saved before receipt evidence was kept has none, so nothing
    /// it owes is quiet.
    pub received: Vec<String>,
    pub unwatched: Option<Unwatched>,
    /// Declaration feedback a wake gate held back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feedback: Vec<Notice>,
    /// The Stop last reported on this binding, until the agent was woken
    /// past it. State saved before Stops were kept has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<Stopped>,
    /// [`SessionState::seen`]. State saved before listings were
    /// remembered has none, so nothing is baseline for what it attends.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub seen: BTreeMap<String, Option<Ms>>,
    /// [`SessionState::starts`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub starts: BTreeMap<String, Option<Ms>>,
}

/// The persisted shapes from before attending was only what an agent
/// declared, read so a monitor restarted across the upgrade keeps every
/// run it registered.
mod legacy {
    use super::*;

    #[derive(Deserialize)]
    pub(super) struct Stored {
        binding: SessionId,
        #[serde(default)]
        attendances: Option<Vec<StoredEntry>>,
        #[serde(default)]
        attendance: Option<Attendance>,
        repeat: Option<RepeatRecord>,
        outstanding: Option<Presentation>,
        #[serde(default)]
        outstanding_episode_broken: bool,
        resolved: Option<(PresentationId, Resolution)>,
        next_seq: u64,
        #[serde(default)]
        next_window: WindowId,
        #[serde(default)]
        received: Vec<String>,
        #[serde(default)]
        unwatched: Option<Unwatched>,
        #[serde(default)]
        feedback: Vec<Notice>,
        #[serde(default)]
        stopped: Option<Stopped>,
        #[serde(default)]
        seen: BTreeMap<String, Option<Ms>>,
        #[serde(default)]
        starts: BTreeMap<String, Option<Ms>>,
    }

    /// A check-in reference as any build saved it. One saved before windows
    /// had identities names none: the presentation carrying it died with
    /// its monitor, so nothing it could spend is lost.
    #[derive(Deserialize)]
    #[serde(untagged)]
    pub(super) enum StoredRef {
        Current {
            window: WindowId,
        },
        /// Keyed by the entry's selector and `since`.
        Keyed(serde::de::IgnoredAny),
    }

    impl From<StoredRef> for CheckInRef {
        fn from(stored: StoredRef) -> Self {
            match stored {
                StoredRef::Current { window } => CheckInRef { window },
                StoredRef::Keyed(_) => CheckInRef { window: 0 },
            }
        }
    }

    /// A check-in as any build saved it. Each state an older build could
    /// leave a check-in in without it reaching the model is still owed.
    #[derive(Deserialize)]
    pub(super) enum StoredCheckIn {
        /// How state saved before attending took effect at once spelled
        /// an entry not yet in force.
        #[serde(alias = "Unarmed")]
        Armed,
        Due,
        Fired,
        Withheld(serde::de::IgnoredAny),
        Undeliverable(serde::de::IgnoredAny),
        ProcessGone,
    }

    impl From<StoredCheckIn> for CheckIn {
        fn from(stored: StoredCheckIn) -> Self {
            match stored {
                StoredCheckIn::Armed => CheckIn::Armed,
                StoredCheckIn::Fired => CheckIn::Fired,
                StoredCheckIn::Due
                | StoredCheckIn::Withheld(_)
                | StoredCheckIn::Undeliverable(_)
                | StoredCheckIn::ProcessGone => CheckIn::Due,
            }
        }
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StoredEntry {
        Current(Entry),
        Listed(Listed),
    }

    /// An entry from when attendance was resolved against a harness's
    /// task listing.
    #[derive(Deserialize)]
    struct Listed {
        selector: Selector,
        #[serde(default)]
        resolved: Option<Resolved>,
        desc: Option<String>,
        since: Ms,
        expect: Ms,
        shell: Option<ShellProof>,
        check_in: CheckIn,
    }

    #[derive(Deserialize)]
    struct Resolved {
        task_id: String,
    }

    #[derive(Deserialize)]
    enum Selector {
        TaskId(String),
        Name(String),
        RunDesc(String),
    }

    impl Selector {
        fn text(&self) -> &str {
            match self {
                Self::TaskId(t) | Self::Name(t) | Self::RunDesc(t) => t,
            }
        }
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
            let mut attendances: Vec<Entry> = match s.attendances {
                Some(entries) => entries.into_iter().map(StoredEntry::current).collect(),
                None => s.attendance.map(entries).unwrap_or_default(),
            };
            let mut next_window = attendances
                .iter()
                .map(|e| e.window.id)
                .max()
                .unwrap_or(0)
                .max(s.next_window);
            for entry in attendances.iter_mut().filter(|e| e.window.id == 0) {
                next_window += 1;
                entry.window.id = next_window;
            }
            Persisted {
                binding: s.binding,
                attendances,
                repeat: s.repeat,
                outstanding: s.outstanding,
                outstanding_episode_broken: s.outstanding_episode_broken,
                resolved: s.resolved,
                next_seq: s.next_seq,
                next_window,
                received: s.received,
                unwatched: s.unwatched,
                feedback: s.feedback,
                stopped: s.stopped,
                seen: s.seen,
                starts: s.starts,
            }
        }
    }

    impl StoredEntry {
        fn current(self) -> Entry {
            match self {
                Self::Current(entry) => entry,
                Self::Listed(l) => entry(
                    l.shell,
                    l.resolved.map(|r| r.task_id),
                    l.desc.unwrap_or_else(|| l.selector.text().to_string()),
                    l.selector.text().to_string(),
                    l.since,
                    l.expect,
                    l.check_in,
                ),
            }
        }
    }

    /// A run keeps its registration, so its end is still recorded. A named
    /// attendance keeps the task a listing resolved it to, so the next
    /// listing still ends it.
    fn entry(
        shell: Option<ShellProof>,
        task: Option<String>,
        desc: String,
        name: String,
        since: Ms,
        expect: Ms,
        check_in: CheckIn,
    ) -> Entry {
        Entry {
            work: match shell {
                Some(proof) => Attending::Run(proof),
                None => Attending::Declared {
                    name,
                    task,
                    kind: None,
                    misses: 0,
                    tracked: Tracked::default(),
                },
            },
            desc,
            started: since,
            window: Window {
                id: 0,
                since,
                expect,
                check_in,
            },
        }
    }

    fn entries(old: Attendance) -> Vec<Entry> {
        match old {
            Attendance::None => Vec::new(),
            Attendance::Intent(r) => vec![r.entry()],
            Attendance::Standing {
                task_id,
                desc,
                since,
                expect,
                shell,
                check_in,
                renewal,
            } => {
                let named = desc.clone().unwrap_or_else(|| task_id.clone());
                let standing = entry(
                    shell,
                    Some(task_id),
                    named.clone(),
                    named,
                    since,
                    expect,
                    check_in,
                );
                std::iter::once(standing)
                    .chain(renewal.map(Request::entry))
                    .collect()
            }
        }
    }

    impl Request {
        fn entry(self) -> Entry {
            let name = self.selector.text().to_string();
            entry(
                self.shell,
                None,
                self.desc.unwrap_or_else(|| name.clone()),
                name,
                self.since,
                self.expect,
                CheckIn::Armed,
            )
        }
    }
}

/// The current park ended without delivering, at `since`. It dates the
/// agent having no waiter and says nothing of its turn: a connection is
/// evidence of reachability only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unwatched {
    pub since: Ms,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionState {
    pub binding: SessionId,
    /// Everything the agent attends. Each entry is independent: adding,
    /// renewing, ending or clearing one never touches another.
    pub attendances: Vec<Entry>,
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
    /// The last window identity this binding gave out.
    pub next_window: WindowId,
    /// The identities of owed work a presentation on this binding was
    /// `Delivered` with, sorted: the only work a qualifying entry may
    /// quiet. Never wider than what is still owed.
    pub received: Vec<String>,
    pub unwatched: Option<Unwatched>,
    /// `NoMatch` / `Ambiguous` for declarations already dropped, not yet
    /// handed to any wake: a gate that forbids waking holds them for the
    /// next Stop that may wake, or that wakes anyway. Handed over once: a
    /// presentation carrying them that fails to emit does not restore them.
    pub feedback: Vec<Notice>,
    /// The Stop last reported on this binding. Only a turn-end report sets
    /// it; a wake clank gave the agent since ends it, and a session start
    /// or a compaction is no evidence either way.
    pub stopped: Option<Stopped>,
    /// Every task id any listing of this binding has shown, with the
    /// earliest hook time it was shown at (`None` while only reports
    /// without one showed it). What a declaration bound later takes its
    /// baseline from.
    pub seen: BTreeMap<String, Option<Ms>>,
    /// The subagents a start was reported for before any declaration was
    /// bound to them, with the latest such start's hook time (`None` once
    /// one came without).
    pub starts: BTreeMap<String, Option<Ms>>,
}

impl SessionState {
    pub fn new(binding: SessionId) -> Self {
        Self {
            binding,
            attendances: Vec::new(),
            repeat: None,
            outstanding: None,
            outstanding_episode_broken: false,
            resolved: None,
            next_seq: 0,
            next_window: 0,
            received: Vec::new(),
            unwatched: None,
            feedback: Vec::new(),
            stopped: None,
            seen: BTreeMap::new(),
            starts: BTreeMap::new(),
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
            next_window: self.next_window,
            received: self.received.clone(),
            unwatched: self.unwatched,
            feedback: self.feedback.clone(),
            stopped: self.stopped.clone(),
            seen: self.seen.clone(),
            starts: self.starts.clone(),
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
            repeat: p.repeat,
            outstanding: None,
            outstanding_episode_broken: false,
            resolved,
            next_seq: p.next_seq,
            next_window: p.next_window,
            received,
            unwatched: p.unwatched,
            feedback: p.feedback,
            stopped: p.stopped,
            seen: p.seen,
            starts: p.starts,
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
            .filter(|e| e.window.check_in == CheckIn::Armed)
            .map(Entry::deadline)
            .min()
    }

    /// When the presentation in flight stops being a delivery in progress.
    pub fn delivery_deadline(&self) -> Option<Ms> {
        self.outstanding.as_ref().map(Presentation::expires)
    }

    fn overdue(&mut self, facts: &Facts<'_>) -> ResultOutcome {
        match self
            .outstanding
            .as_ref()
            .filter(|p| facts.now >= p.expires())
            .map(|p| p.id.clone())
        {
            Some(id) => self.settle(id, Resolution::Failed, facts),
            None => ResultOutcome::InTime,
        }
    }

    /// Whether this state belongs to the facts' binding. A state for
    /// another session must be replaced, never consulted.
    pub fn bound(&self, facts: &Facts<'_>) -> bool {
        facts.binding == Some(&self.binding)
    }

    /// Every entry, with what [`qualifies`] says of it now.
    pub fn standing<'s>(&'s self, facts: &Facts<'_>) -> Vec<(&'s Entry, Qualifies)> {
        self.attendances
            .iter()
            .map(|e| (e, qualifies(e, facts)))
            .collect()
    }

    /// Every run whose registered process is positively gone, as its
    /// terminal record reads; one whose process cannot be seen is kept. Nothing is removed: the monitor writes each record durably
    /// first and only then calls [`Self::end_run`], so a crash between the
    /// two writes the same record again rather than none.
    pub fn ended_runs(&self, liveness: &dyn Liveness, now: Ms) -> Vec<RunEnd> {
        self.attendances
            .iter()
            .filter_map(|e| {
                let proof = e
                    .proof()
                    .filter(|p| liveness.presence(p) == Presence::Gone)?;
                Some(RunEnd {
                    proof: proof.clone(),
                    desc: e.desc.clone(),
                    started: e.started,
                    ended_seen: now,
                })
            })
            .collect()
    }

    /// The run `proof` registered is over and its record is written.
    pub fn end_run(&mut self, proof: &ShellProof) {
        self.retain_entries(|e| e.proof() != Some(proof));
    }

    pub fn step(&mut self, event: Event, facts: &Facts<'_>) -> Output {
        self.prune_received(facts);
        self.forget_stale_repeat(facts);
        self.fall_due(facts);
        let out = match event {
            Event::TurnEnd(report) => Output::Verdict(self.turn_end(report, facts)),
            Event::Attend(req) => Output::Attend(self.attend(req, facts)),
            Event::Delivered(id) => Output::Result(self.settle(id, Resolution::Delivered, facts)),
            Event::EmitFailed(id) => Output::Result(self.settle(id, Resolution::Failed, facts)),
            Event::ResultOverdue => Output::Result(self.overdue(facts)),
            Event::CheckInDue | Event::Observed => Output::Effects(Vec::new()),
            Event::Parked(report) => Output::Verdict(self.parked(report, facts, Carrier::Held)),
            Event::Compacted(report) => {
                Output::Verdict(self.parked(report, facts, Carrier::Compacted))
            }
            Event::SessionStart(session) => Output::Verdict(self.session_start(session, facts)),
            Event::NativeTurnEnd(handoff) => Output::Verdict(self.native_turn_end(handoff, facts)),
        };
        // A notice wakes the agent with no result to wait for: it is past
        // its Stop as soon as the hook is told.
        if matches!(out, Output::Verdict(Verdict::Notify(_))) {
            self.stopped = None;
        }
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
    /// work.
    fn quieting(&self, facts: &Facts<'_>) -> bool {
        self.attendances
            .iter()
            .any(|e| qualifies(e, facts) == Qualifies::Yes)
    }

    /// A clear or a run's end removes entries through here, and says how
    /// many it removed. When the last entry leaves, what the set quieted is
    /// released: that work is presented again before any later entry can
    /// quiet it.
    fn retain_entries(&mut self, keep: impl FnMut(&Entry) -> bool) -> usize {
        let before = self.attendances.len();
        self.attendances.retain(keep);
        if before > 0 && self.attendances.is_empty() {
            self.received.clear();
        }
        before - self.attendances.len()
    }

    /// Apply one report of the bound session to what it attends by
    /// declaration. Runs are untouched, and no listed task creates or names
    /// anything: a task no declaration matches is only remembered as seen.
    ///
    /// A pending declaration binds to its one live match in any listing
    /// (merging into a declaration already bound to that task), is dropped
    /// with a notice when several match or its one match has no id to
    /// track, and with another when the agent's own Stops have gone
    /// [`MISSES_ALLOWED`] listings without it. A bound one moves by
    /// [`transition`], and leaves when that ends it. The notices join
    /// [`Self::feedback`].
    fn observe(&mut self, at: Option<Ms>, source: Source<'_>) {
        let (tasks, stopping) = match source {
            Source::ChildStart { child } => return self.child_started(child, at),
            Source::AgentStop { tasks } => (tasks, None),
            Source::ChildStop {
                child,
                tasks,
                agent,
            } => (tasks, Some((child, agent))),
        };
        let Some(live) = tasks.live() else {
            return;
        };
        let listed = |id: &str| live.iter().any(|t| t.id.as_deref() == Some(id));
        let mut notices = Vec::new();
        let mut merges: Vec<Merge> = Vec::new();
        let held: Vec<String> = self
            .attendances
            .iter()
            .filter_map(|e| e.task().map(str::to_owned))
            .collect();
        let summaries = || live.iter().copied().map(TaskSummary::from).collect();
        let mut drop = vec![false; self.attendances.len()];
        for (i, entry) in self.attendances.iter_mut().enumerate() {
            let Attending::Declared {
                name,
                task,
                kind,
                misses,
                tracked,
            } = &mut entry.work
            else {
                continue;
            };
            if task.is_none() {
                let matches: Vec<&BackgroundTask> = live
                    .iter()
                    .copied()
                    .filter(|t| t.description.as_deref() == Some(name.as_str()))
                    .collect();
                match matches.as_slice() {
                    [BackgroundTask { id: Some(id), .. }] if held.contains(id) => {
                        merges.push(Merge {
                            task: id.clone(),
                            window: entry.window.clone(),
                            desc: entry.desc.clone(),
                        });
                        drop[i] = true;
                    }
                    [matched @ BackgroundTask { id: Some(id), .. }] => {
                        let started = self.starts.remove(id);
                        let own_stop = stopping.is_some_and(|(child, _)| child == id);
                        *tracked = Tracked {
                            baseline: baseline(&self.seen, at),
                            as_of: started.flatten(),
                            // Its own stop cannot be ordered after a start
                            // remembered for it except by that start's time.
                            phase: match (own_stop, started) {
                                (true, Some(since)) => Phase::Unsettled { since },
                                _ => Phase::Live,
                            },
                        };
                        *task = Some(id.clone());
                        *kind = Some(matched.kind.clone().unwrap_or_default());
                        *misses = 0;
                    }
                    [nameless] => {
                        notices.push(Notice::Untrackable {
                            name: name.clone(),
                            task: TaskSummary::from(*nameless),
                        });
                        drop[i] = true;
                    }
                    [] if stopping.is_none() => {
                        *misses = misses.saturating_add(1);
                        if *misses >= MISSES_ALLOWED {
                            notices.push(Notice::NoMatch {
                                name: name.clone(),
                                live: summaries(),
                            });
                            drop[i] = true;
                        }
                    }
                    [] => {}
                    many => {
                        notices.push(Notice::Ambiguous {
                            name: name.clone(),
                            matches: many.iter().copied().map(TaskSummary::from).collect(),
                        });
                        drop[i] = true;
                    }
                }
            }
            let Some(id) = task.as_deref().filter(|_| !drop[i]) else {
                continue;
            };
            if !Subject::of(kind).follows_lifecycle(tracked) {
                drop[i] = !listed(id);
                continue;
            }
            let candidates = || candidates(&live, &tracked.baseline, id);
            let sight = match stopping {
                Some((child, agent)) if child == id => {
                    let candidates = candidates();
                    Sight::Stopped {
                        sure: self
                            .stopped
                            .as_ref()
                            .is_some_and(|stop| excludes_agent(stop, at, agent, &candidates)),
                        candidates,
                    }
                }
                _ if listed(id) => Sight::Listed,
                _ => Sight::Absent {
                    agent_stop: stopping.is_none(),
                    live: live.iter().filter_map(|t| t.id.clone()).collect(),
                    candidates: candidates(),
                },
            };
            tracked.apply(at, sight);
            drop[i] = tracked.phase == Phase::Ended;
        }
        let mut dropping = drop.into_iter();
        self.retain_entries(|_| !dropping.next().unwrap_or(false));
        for m in merges {
            if let Some(held) = self
                .attendances
                .iter_mut()
                .find(|e| e.task() == Some(&m.task))
            {
                held.window = m.window;
                held.desc = m.desc;
            }
        }
        self.feedback.append(&mut notices);
        for id in live.iter().filter_map(|t| t.id.as_ref()) {
            let first = self.seen.entry(id.clone()).or_insert(at);
            *first = match (*first, at) {
                (Some(known), Some(at)) => Some(known.min(at)),
                (known, at) => known.or(at),
            };
        }
    }

    /// A start names no description, so it binds nothing: it moves the
    /// declaration already bound to `child`, or is remembered for the
    /// listing that binds one.
    fn child_started(&mut self, child: &str, at: Option<Ms>) {
        let bound = self.attendances.iter_mut().find_map(|e| match &mut e.work {
            Attending::Declared {
                task: Some(task),
                tracked,
                ..
            } if task == child => Some(tracked),
            _ => None,
        });
        match bound {
            Some(tracked) => tracked.apply(at, Sight::Started),
            None => {
                let latest = self.starts.entry(child.to_string()).or_insert(at);
                *latest = (*latest).zip(at).map(|(known, at)| known.max(at));
            }
        }
    }

    /// Apply a subagent's own report, and say whether it was this
    /// binding's. It is evidence about declarations only: no Stop is
    /// recorded, nothing is presented and no verdict is owed, so it needs
    /// no knowledge of what is owed.
    pub fn child(&mut self, report: &ChildReport, facts: &Facts<'_>) -> bool {
        if facts.binding != Some(&report.session) || !self.bound(facts) {
            return false;
        }
        let source = match &report.seen {
            ChildSeen::Started => Source::ChildStart {
                child: &report.child,
            },
            ChildSeen::Stopped { tasks, agent } => Source::ChildStop {
                child: &report.child,
                tasks,
                agent,
            },
        };
        self.observe(report.hook_started, source);
        true
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
        let owed = owed_identities(&self.deliverable(facts));
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

    fn turn_end(&mut self, report: TurnEndReport, facts: &Facts<'_>) -> Verdict {
        if facts.binding != Some(&report.session) || !self.bound(facts) {
            return Verdict::Silent(SilentWhy::StaleBinding);
        }
        self.record_stop(&report, facts.now);
        self.observe(
            report.hook_started,
            Source::AgentStop {
                tasks: &report.tasks,
            },
        );
        if let Some(gated) = drive_gate(facts) {
            return gated;
        }
        if report.tool == Tool::Grok {
            return grok_verdict();
        }
        let verdict = release_for_user_command(
            self.deliver_or_park(facts, Some(&report.place), Carrier::Stop),
            facts,
        );
        self.with_feedback(verdict, facts)
    }

    /// Every Stop of the bound session is kept, placed or not: one the
    /// adapter could not place is still a turn that ended, and replaces
    /// whatever the last one said. The same report admitted again, by a
    /// hook that attached to another monitor or a replay, is the same
    /// Stop and keeps the time it was first reported; one that names no
    /// turn and saw no record cannot be told from a later one, and is
    /// dated afresh.
    fn record_stop(&mut self, report: &TurnEndReport, now: Ms) {
        let place = &report.place;
        let again = self.stopped.as_ref().is_some_and(|held| {
            (held.turn.is_some() || held.read_to.is_some())
                && held.turn == place.turn
                && held.position == place.position
                && held.read_to == place.read_to
        });
        if !again {
            self.stopped = Some(Stopped {
                turn: place.turn.clone(),
                position: place.position,
                read_to: place.read_to,
                at: now,
                hook_started: report.hook_started,
                listed: report
                    .tasks
                    .live()
                    .map(|live| live.iter().filter_map(|t| t.id.clone()).collect()),
            });
        }
    }

    /// Grok's native Stop: its listing does for declarations what a Stop's
    /// does, and the waiter verdict is [`handoff_verdict`]'s. It is Grok's
    /// one evidence that a turn has ended, so it, never the background
    /// wait, carries the check-ins owed, beside any request for that wait.
    fn native_turn_end(&mut self, handoff: Handoff, facts: &Facts<'_>) -> Verdict {
        let bound = facts.binding == Some(&handoff.session) && self.bound(facts);
        if bound {
            self.observe(
                handoff.hook_started,
                Source::AgentStop {
                    tasks: &handoff.tasks,
                },
            );
        }
        let verdict = match handoff_verdict(&handoff, facts) {
            waiting @ (Verdict::Notify(_) | Verdict::Silent(SilentWhy::WaitArmed)) if bound => {
                self.check_in_at_turn_end(waiting, facts)
            }
            other => other,
        };
        self.with_feedback(verdict, facts)
    }

    /// `verdict`, a turn end's answer that carries no work, joined by every
    /// check-in owed as one new presentation, when one may wake the agent
    /// and none is in flight.
    fn check_in_at_turn_end(&mut self, verdict: Verdict, facts: &Facts<'_>) -> Verdict {
        let lines = self.carried_check_ins(Carrier::NativeStop, None, false, facts);
        if lines.is_empty() {
            return verdict;
        }
        let p = self.present(&[], &lines, None, facts.now);
        let mut notices = vec![Notice::CheckIn(lines)];
        if let Verdict::Notify(mut asked) = verdict {
            notices.append(&mut asked);
        }
        let verdict = Verdict::Deliver {
            presentation: p.id.clone(),
            identities: Vec::new(),
            repeat: 0,
            escalated: false,
            notices,
        };
        self.outstanding = Some(p);
        verdict
    }

    /// Held feedback joins a verdict that wakes the agent anyway. It turns
    /// one that would not wake into a wake only where
    /// [`may_wake_without_work`]; otherwise it is held for a later Stop. A
    /// turn [`release_for_user_command`] already ended is one such silence.
    fn with_feedback(&mut self, verdict: Verdict, facts: &Facts<'_>) -> Verdict {
        if self.feedback.is_empty() {
            return verdict;
        }
        let wakes = match &verdict {
            Verdict::Deliver { .. } | Verdict::Notify(_) => true,
            Verdict::Park | Verdict::Silent(SilentWhy::WaitArmed) => may_wake_without_work(facts),
            Verdict::Silent(
                SilentWhy::StaleBinding | SilentWhy::AutoOff | SilentWhy::UserCommand,
            )
            | Verdict::Failed(_) => false,
        };
        if !wakes {
            return verdict;
        }
        with_notices(verdict, std::mem::take(&mut self.feedback))
    }

    fn parked(&mut self, report: TurnEndReport, facts: &Facts<'_>, carrier: Carrier) -> Verdict {
        if facts.binding != Some(&report.session) || !self.bound(facts) {
            return Verdict::Silent(SilentWhy::StaleBinding);
        }
        if let Some(gated) = drive_gate(facts) {
            return gated;
        }
        if report.tool == Tool::Grok {
            return grok_verdict();
        }
        let verdict = release_for_user_command(
            self.deliver_or_park(facts, Some(&report.place), carrier),
            facts,
        );
        self.with_feedback(verdict, facts)
    }

    fn session_start(&mut self, session: SessionId, facts: &Facts<'_>) -> Verdict {
        if facts.binding != Some(&session) || !self.bound(facts) {
            return Verdict::Silent(SilentWhy::StaleBinding);
        }
        if let Some(gated) = drive_gate(facts) {
            return gated;
        }
        self.deliver_or_park(facts, None, Carrier::SessionStart)
    }

    /// The [`deliverable`](Self::deliverable) work and, at a park, every
    /// check-in owed, as one new presentation; or Park when there is
    /// neither or another presentation is in flight. A park keeps the
    /// hook's wait armed, so work that arrives later still wakes the agent.
    /// `place` is the Stop it counts against; without one it is not
    /// counted.
    fn deliver_or_park(
        &mut self,
        facts: &Facts<'_>,
        place: Option<&TurnPlace>,
        carrier: Carrier,
    ) -> Verdict {
        if self.outstanding.is_some() {
            return Verdict::Park;
        }
        let deliverable = self.deliverable(facts);
        let lines = self.carried_check_ins(carrier, place, !deliverable.is_empty(), facts);
        if deliverable.is_empty() && lines.is_empty() {
            return Verdict::Park;
        }
        let p = self.present(&deliverable, &lines, place, facts.now);
        let repeat = p.repeat.count();
        let verdict = Verdict::Deliver {
            presentation: p.id.clone(),
            identities: p.identities.clone(),
            repeat,
            escalated: repeat >= ESCALATE_AT,
            notices: match lines.is_empty() {
                true => Vec::new(),
                false => vec![Notice::CheckIn(lines)],
            },
        };
        self.outstanding = Some(p);
        verdict
    }

    /// Always a new presentation, counted from the last COMMITTED repeat
    /// record. A failed one is never reused: its count belongs to the
    /// episode it was made in, which an observed change may since have
    /// ended. One carrying no work counts nothing.
    fn present(
        &mut self,
        owed: &[OwedItem],
        check_ins: &[CheckInLine],
        place: Option<&TurnPlace>,
        now: Ms,
    ) -> Presentation {
        let identities = owed_identities(owed);
        let receipts: Vec<String> = owed.iter().flat_map(|o| o.receipts.clone()).collect();
        self.next_seq += 1;
        Presentation {
            id: PresentationId {
                session: self.binding.clone(),
                seq: self.next_seq,
            },
            repeat: match place {
                Some(place) if !identities.is_empty() => {
                    count_delivery(self.repeat.as_ref(), &identities, place)
                }
                _ => RepeatDecision::Uncounted,
            },
            identities,
            receipts,
            handed: now,
            check_ins: check_ins.iter().map(|l| l.check_in.clone()).collect(),
        }
    }

    /// The check-ins a presentation made for `carrier`, reported at `place`
    /// and carrying work or not, may carry: every one owed, when no other
    /// presentation is in flight, the carrier is at a turn's end, and it
    /// carries work or may wake the agent without it. The one gate every
    /// carrier passes.
    fn carried_check_ins(
        &self,
        carrier: Carrier,
        place: Option<&TurnPlace>,
        with_work: bool,
        facts: &Facts<'_>,
    ) -> Vec<CheckInLine> {
        let at_turn_end = match carrier {
            Carrier::Stop | Carrier::Held | Carrier::NativeStop => true,
            Carrier::Compacted => {
                place.is_some_and(|p| self.stopped.as_ref().is_some_and(|s| s.stands_in(p)))
            }
            Carrier::SessionStart => false,
        };
        match self.outstanding.is_none()
            && at_turn_end
            && (with_work || may_wake_without_work(facts))
        {
            true => self.owed_check_ins(facts),
            false => Vec::new(),
        }
    }

    /// Every entry whose expectation has passed is owed its check-in from
    /// now on. Not gated on [`qualifies`]: at the deadline it is false by
    /// definition, and the check-in is exactly the wake that deadline owes.
    fn fall_due(&mut self, facts: &Facts<'_>) {
        if !self.bound(facts) {
            return;
        }
        for entry in &mut self.attendances {
            if entry.window.check_in == CheckIn::Armed && facts.now >= entry.deadline() {
                entry.window.check_in = CheckIn::Due;
            }
        }
    }

    /// The lines of every check-in owed now, in one. A run whose process
    /// is gone is left out: the run's end is the wake it owes.
    fn owed_check_ins(&self, facts: &Facts<'_>) -> Vec<CheckInLine> {
        self.attendances
            .iter()
            .filter(|e| e.window.check_in == CheckIn::Due)
            .filter(|e| {
                e.proof()
                    .is_none_or(|p| facts.liveness.presence(p) != Presence::Gone)
            })
            .map(|e| check_in_line(e, facts.now))
            .collect()
    }

    fn attend(&mut self, req: AttendRequest, facts: &Facts<'_>) -> AttendReply {
        let session = match &req {
            AttendRequest::Run { session, .. }
            | AttendRequest::Declare { session, .. }
            | AttendRequest::Clear { session, .. } => session,
        };
        if facts.binding != Some(session) || !self.bound(facts) {
            return AttendReply::RefusedStale;
        }
        match req {
            AttendRequest::Run {
                proof,
                desc,
                expect,
                ..
            } => {
                let at = |e: &Entry| e.proof() == Some(&proof);
                let work = Attending::Run(proof.clone());
                self.add(at, work, desc, expect, facts.now)
            }
            AttendRequest::Declare {
                name, desc, expect, ..
            } => {
                // A declaration already bound to a task is not renewed by
                // name: which task the name means now is the next listing's
                // to say, and a same-task match merges into it there.
                let at = |e: &Entry| e.name() == name && e.task().is_none();
                let work = Attending::declared(name.clone());
                let desc = desc.unwrap_or_else(|| name.clone());
                self.add(at, work, desc, expect, facts.now)
            }
            AttendRequest::Clear { which, .. } => self.clear(which),
        }
    }

    /// A request renews every entry `same` picks, or adds `work` when
    /// there is none. It never removes or renews any other.
    fn add(
        &mut self,
        same: impl Fn(&Entry) -> bool,
        work: Attending,
        desc: String,
        expect: Ms,
        now: Ms,
    ) -> AttendReply {
        let mut renewed = false;
        for i in 0..self.attendances.len() {
            if !same(&self.attendances[i]) {
                continue;
            }
            let window = self.open_window(now, expect);
            let entry = &mut self.attendances[i];
            let desc = entry.proof().is_none().then(|| desc.clone());
            entry.renew(window, desc);
            renewed = true;
        }
        if renewed {
            return AttendReply::Renewed;
        }
        let window = self.open_window(now, expect);
        self.attendances.push(Entry {
            work,
            desc,
            started: now,
            window,
        });
        AttendReply::Accepted
    }

    /// A new window from `since`, its check-in armed, with an identity no
    /// window of this binding has had.
    fn open_window(&mut self, since: Ms, expect: Ms) -> Window {
        self.next_window += 1;
        Window {
            id: self.next_window,
            since,
            expect,
            check_in: CheckIn::Armed,
        }
    }

    fn clear(&mut self, which: Clearing) -> AttendReply {
        let removed = match which {
            Clearing::All => self.retain_entries(|_| false),
            Clearing::Run(proof) => self.retain_entries(|e| e.proof() != Some(&proof)),
            Clearing::Named(name) => self.retain_entries(|e| e.name() != name),
        };
        AttendReply::Cleared { removed }
    }

    /// Settle the in-flight presentation with its one result. A success
    /// commits its receipts and its repeat decision, which was made
    /// against the record nothing could replace while it was in flight,
    /// and is the only evidence that its work was received. It spends the
    /// check-ins it carried, but only for entries still in the expectation
    /// it was presented for: one renewed meanwhile keeps its new one. The
    /// agent starts a turn on it.
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
        self.stopped = None;
        for entry in &mut self.attendances {
            if p.check_ins.iter().any(|r| still_owed(entry, r)) {
                entry.window.check_in = CheckIn::Fired;
            }
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

    /// Why recovery must leave this agent alone, if it must. Recovery has
    /// no rule of its own: attendance holds it only while it quiets every
    /// owed item, by the same deliverable projection a turn-end presents
    /// from, so work the session never received stays recoverable.
    /// `facts.owed` must be the work actually owed: none owed is quiet.
    ///
    /// A pending block is not asked about: what it withholds is already
    /// absent from `facts.owed`, and what it does not withhold is owed
    /// whoever raised the block.
    pub fn recovery_quiet(&self, facts: &Facts<'_>) -> Option<Quiet> {
        if !self.bound(facts) {
            return None;
        }
        if facts.asking {
            return Some(Quiet::Asking);
        }
        (self.quieting(facts) && self.deliverable(facts).is_empty()).then_some(Quiet::Attending)
    }
}

/// One report of the bound session, as declarations read it.
enum Source<'a> {
    /// The agent's own Stop.
    AgentStop {
        tasks: &'a TaskSnapshot,
    },
    ChildStop {
        child: &'a str,
        tasks: &'a TaskSnapshot,
        /// The agent's record as the child's hook read it.
        agent: &'a TurnPlace,
    },
    ChildStart {
        child: &'a str,
    },
}

/// The ids a listing taken at `at` cannot have been the first to show:
/// those some report showed strictly earlier. Hook times order it, never
/// arrival, so a listing that arrives first but was taken later adds
/// nothing, and neither does one with no time.
fn baseline(seen: &BTreeMap<String, Option<Ms>>, at: Option<Ms>) -> Vec<String> {
    seen.iter()
        .filter(|(_, first)| first.zip(at).is_some_and(|(first, at)| first < at))
        .map(|(id, _)| id.clone())
        .collect()
}

/// What a bound declaration's task is, by the kind its record keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Subject {
    Subagent,
    /// A shell, a monitor, or a task the listing gave no kind.
    Other,
    /// Bound before kinds were kept.
    Unknown,
}

impl Subject {
    fn of(kind: &Option<String>) -> Self {
        match kind.as_deref() {
            Some("subagent") => Subject::Subagent,
            Some(_) => Subject::Other,
            None => Subject::Unknown,
        }
    }

    /// Whether the declaration ends by [`transition`] rather than when its
    /// task leaves the listing. Only a subagent stops and resumes under one
    /// id while work it started runs. Of the records with no kind, one the
    /// subagent table has touched was written by the build that first kept
    /// a subagent's phase, and wrote no kind: ending it on absence would
    /// forget a stored park or an unsettled resume. One it never touched is
    /// from before that table, and ended when its task left the listing.
    fn follows_lifecycle(self, tracked: &Tracked) -> bool {
        match self {
            Subject::Subagent => true,
            Subject::Other => false,
            Subject::Unknown => !tracked.is_unobserved(),
        }
    }
}

/// The listed tasks subagent `child` may have started: not older than it,
/// not itself, not another subagent, and not the session's own wait.
fn candidates(live: &[&BackgroundTask], baseline: &[String], child: &str) -> Vec<HeldTask> {
    live.iter()
        .filter(|t| t.kind.as_deref() != Some("subagent"))
        .filter(|t| !is_clank_wait(t))
        .filter_map(|t| {
            let id = t.id.as_ref()?;
            (id != child && !baseline.contains(id)).then(|| HeldTask {
                id: id.clone(),
                kind: t.kind.clone(),
                description: t.description.clone(),
            })
        })
        .collect()
}

/// Whether `task` runs `clank wait`: its command's program is `clank`, by
/// any path, and its first argument is `wait`. Such a task is the agent's
/// own waiter, never a subagent's work: subagents are told never to run
/// one, and every Grok turn backgrounds one, so counting it would hold
/// every finished subagent until its check-in. Matched by words, so a
/// command that merely mentions it (`echo clank wait`) is not one.
fn is_clank_wait(task: &BackgroundTask) -> bool {
    let Some(command) = task.raw.get("command").and_then(|c| c.as_str()) else {
        return false;
    };
    let command = command.trim_start();
    let (program, rest) = match command.chars().next() {
        Some(quote @ ('"' | '\'')) => match command[1..].split_once(quote) {
            Some((program, rest)) => (program, rest),
            None => return false,
        },
        _ => command
            .split_once(char::is_whitespace)
            .unwrap_or((command, "")),
    };
    program.rsplit('/').next() == Some("clank") && rest.split_whitespace().next() == Some("wait")
}

/// Whether the agent could not have started any of `candidates`, listed
/// by a child's hook at `at`: the agent's last Stop came first, `record`
/// (its own record as that child's hook read it) is still what the Stop
/// saw, and the Stop's listing had none of them. Judged by what the two
/// hooks saw, never by where the agent is when the report arrives.
fn excludes_agent(
    stop: &Stopped,
    at: Option<Ms>,
    record: &TurnPlace,
    candidates: &[HeldTask],
) -> bool {
    let earlier = stop
        .hook_started
        .zip(at)
        .is_some_and(|(stop, at)| stop < at);
    let unlisted = stop
        .listed
        .as_ref()
        .is_some_and(|listed| candidates.iter().all(|c| !listed.contains(&c.id)));
    earlier && stop.stands_in(record) && unlisted
}

/// `entry`'s line in a check-in at `now`.
fn check_in_line(entry: &Entry, now: Ms) -> CheckInLine {
    CheckInLine {
        check_in: CheckInRef {
            window: entry.window.id,
        },
        desc: entry.desc.clone(),
        name: entry.name().to_string(),
        run: entry.proof().is_some(),
        elapsed: now.saturating_sub(entry.window.since),
        expect: entry.window.expect,
    }
}

/// Whether `entry` still holds the window `presented` was presented for,
/// and still owes it.
fn still_owed(entry: &Entry, presented: &CheckInRef) -> bool {
    entry.window.check_in == CheckIn::Due && entry.window.id == presented.window
}

/// What a presentation is made for, by which report carries it. A Stop
/// as it is reported, and the very report a park holds for it, are the
/// agent sitting at a turn's end with its hook passing on what it is told,
/// and so is Grok's native Stop. A compaction is a new report, at that turn's end only while both it and
/// the Stop read a record and it has not moved ([`Stopped::stands_in`]):
/// missing or contradicting placement is no evidence. Only those carry a
/// check-in; a session start's context may never reach the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Carrier {
    Stop,
    Held,
    Compacted,
    /// Grok's native Stop, which answers at once and parks nothing.
    NativeStop,
    SessionStart,
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

/// `notices` joined to a waking verdict, or alone in place of one that
/// would not wake. Whether they may wake at all is
/// [`SessionState::with_feedback`]'s to decide.
fn with_notices(verdict: Verdict, mut notices: Vec<Notice>) -> Verdict {
    if notices.is_empty() {
        return verdict;
    }
    match verdict {
        Verdict::Deliver {
            presentation,
            identities,
            repeat,
            escalated,
            notices: mut carried,
        } => {
            carried.append(&mut notices);
            Verdict::Deliver {
                presentation,
                identities,
                repeat,
                escalated,
                notices: carried,
            }
        }
        Verdict::Notify(mut carried) => {
            carried.append(&mut notices);
            Verdict::Notify(carried)
        }
        Verdict::Park | Verdict::Silent(SilentWhy::WaitArmed | SilentWhy::UserCommand) => {
            Verdict::Notify(notices)
        }
        gated @ (Verdict::Silent(SilentWhy::StaleBinding | SilentWhy::AutoOff)
        | Verdict::Failed(_)) => gated,
    }
}

/// Whether clank may wake the agent with something that is not work: every
/// gate a wake passes ([`wake_gate`]), and no person's command waiting for
/// the idle prompt that wake would take.
fn may_wake_without_work(facts: &Facts<'_>) -> bool {
    wake_gate(facts).is_ok() && !facts.user_command
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
    Asking,
    Attending,
}

/// Grok's native Stop: whether the monitor holds a live registration of
/// this session's background `clank wait`, and whether this Stop is the
/// continuation of one that asked for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    pub session: SessionId,
    pub wait_registered: bool,
    pub continued: bool,
    /// Grok's `backgroundTasks`, read only for declarations.
    #[serde(default)]
    pub tasks: TaskSnapshot,
    /// As [`TurnEndReport::hook_started`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_started: Option<Ms>,
}

/// Grok's native turn-end, with or without a monitor: the drive gate, then
/// a request for the background wait unless one already carries the wake.
///
/// A continuation is the turn that was just asked to arm one, so its wait
/// may have started without registering yet: it is pending, never asked
/// for again. That is also what stops a wait that failed from being
/// requested in a loop within one turn.
pub fn handoff_verdict(handoff: &Handoff, facts: &Facts<'_>) -> Verdict {
    if facts.binding != Some(&handoff.session) {
        return Verdict::Silent(SilentWhy::StaleBinding);
    }
    if let Some(gated) = drive_gate(facts) {
        return gated;
    }
    if handoff.wait_registered || handoff.continued {
        return Verdict::Silent(SilentWhy::WaitArmed);
    }
    Verdict::Notify(vec![Notice::ArmWait])
}

/// What a hook with no monitor says to `session`: nothing unless clank
/// drives it, and then `notice`. Never work and never a park, which only a
/// monitor decides.
pub fn unmonitored_verdict(session: &SessionId, facts: &Facts<'_>, notice: Notice) -> Verdict {
    if facts.binding != Some(session) {
        return Verdict::Silent(SilentWhy::StaleBinding);
    }
    if let Some(gated) = drive_gate(facts) {
        return gated;
    }
    Verdict::Notify(vec![notice])
}

#[cfg(test)]
mod tests;
