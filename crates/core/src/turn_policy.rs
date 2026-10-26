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

use crate::hook_io::{BackgroundTask, HookInput, clank_run_desc};
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
    TaskId(String),
    /// A task's `description`, matched exactly: a subagent's name.
    Name(String),
    /// The `--desc` of a `clank run`.
    RunDesc(String),
}

impl Selector {
    fn matches(&self, task: &BackgroundTask) -> bool {
        match self {
            Self::TaskId(id) => task.id.as_deref() == Some(id),
            Self::Name(name) => task.description.as_deref() == Some(name),
            Self::RunDesc(desc) => task
                .command
                .as_deref()
                .and_then(clank_run_desc)
                .is_some_and(|d| &d == desc),
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

/// The one check-in an attendance can owe. It is armed only once the
/// attendance has qualified at a turn-end: one that never qualified quieted
/// nothing, so no wake was handed to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckIn {
    Unarmed,
    Armed,
    Fired,
    /// Due while a gate held it back. Spent, not deferred: what lifts the
    /// gate (the person's answer, auto turning on) is itself a wake, and
    /// past the deadline the attendance no longer quiets recovery.
    Withheld(Withheld),
    /// Fired, and the route could not carry it. Never counted as
    /// delivered; past the deadline recovery treats the agent as any
    /// other.
    Undeliverable(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Withheld {
    AutoOff,
    Asking,
    Blocked,
}

/// Which attendance a check-in was fired for: its task, and the moment
/// that attendance was accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckInRef {
    pub task_id: String,
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

/// An accepted attendance request, not yet resolved to a task: the task
/// it names may not exist in any report yet. Its clock runs from `since`,
/// the moment it was accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub selector: Selector,
    pub desc: Option<String>,
    pub since: Ms,
    pub expect: Ms,
    pub shell: Option<ShellProof>,
}

/// An agent's attendance. Only a new request renews its clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Attendance {
    None,
    Intent(Request),
    /// Resolved to one task. From here its identity is the task id.
    Standing {
        task_id: String,
        kind: TaskKind,
        desc: Option<String>,
        since: Ms,
        expect: Ms,
        shell: Option<ShellProof>,
        check_in: CheckIn,
        /// A request made while this stands. This attendance, its
        /// expectation and its check-in stay in force until a Stop
        /// resolves the request: to this task, it renews them; to anything
        /// else, it ends this attendance.
        #[serde(default)]
        renewal: Option<Request>,
    },
}

impl Attendance {
    /// The task whose received work this attendance holds. Work is
    /// identified by its resolved task id, never by how it was named.
    pub fn work(&self) -> Option<&str> {
        match self {
            Self::None | Self::Intent(_) => None,
            Self::Standing { task_id, .. } => Some(task_id),
        }
    }
}

/// The provider wakes the agent when its background work completes.
/// Without that wake, quiet on an attendance would leave the agent with
/// no wake at all.
pub fn completion_wake(tool: Tool) -> bool {
    matches!(tool, Tool::Claude)
}

/// Why an attendance does or does not quiet anything.
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

/// THE rule. Turn-end, check-in and recovery all ask this and nothing
/// else whether an attendance may quiet anything.
pub fn qualifies(
    attendance: &Attendance,
    tool: Tool,
    snapshot: Option<&TaskSnapshot>,
    facts: &Facts<'_>,
) -> Qualifies {
    let Attendance::Standing {
        task_id,
        kind,
        since,
        expect,
        shell,
        ..
    } = attendance
    else {
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
        TaskKind::Shell => match shell {
            None => return Qualifies::No(Unqualified::NoProcessProof),
            Some(proof) if !facts.liveness.holds(proof) => {
                return Qualifies::No(Unqualified::ProcessGone);
            }
            Some(_) => {}
        },
        TaskKind::Unknown(_) => return Qualifies::No(Unqualified::UnknownKind),
    }
    if facts.now >= since.saturating_add(*expect) {
        return Qualifies::No(Unqualified::PastDeadline);
    }
    Qualifies::Yes
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
    /// A shell task resolved with no recorded process: only `clank run`
    /// (or `--pid`) can prove one.
    ShellNeedsRun { task_id: String },
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
}

impl From<&BackgroundTask> for TaskSummary {
    fn from(t: &BackgroundTask) -> Self {
        Self {
            id: t.id.clone(),
            kind: TaskKind::of(t),
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
        notice: Option<Notice>,
    },
    /// Wake with text that is not work. Creates no presentation.
    Notify(Notice),
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
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttendReply {
    /// Accepted as an intent; proof comes at the next turn-end.
    Accepted,
    Cleared {
        had: bool,
    },
    RefusedStale,
}

/// A side effect the monitor performs for a transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    CommitReceipts(Vec<String>),
    CommitRepeat(Option<RepeatRecord>),
    /// Deliver the one check-in through the nudge transport.
    CheckIn {
        check_in: CheckInRef,
        desc: Option<String>,
        elapsed: Ms,
        expect: Ms,
    },
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
    /// The check-in fired for `check_in` could not be delivered, and why.
    CheckInUndeliverable {
        check_in: CheckInRef,
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
/// restored attendance is unproven until the next turn-end reports one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Persisted {
    pub binding: SessionId,
    pub attendance: Attendance,
    pub repeat: Option<RepeatRecord>,
    pub outstanding: Option<Presentation>,
    #[serde(default)]
    pub outstanding_episode_broken: bool,
    pub resolved: Option<(PresentationId, Resolution)>,
    pub next_seq: u64,
    /// State saved before receipt evidence was kept has none, so nothing
    /// it owes is quiet.
    #[serde(default)]
    pub received: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionState {
    pub binding: SessionId,
    pub attendance: Attendance,
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
    /// `Delivered` with, sorted: the only work a qualifying attendance may
    /// quiet. Never wider than what is still owed.
    pub received: Vec<String>,
}

impl SessionState {
    pub fn new(binding: SessionId) -> Self {
        Self {
            binding,
            attendance: Attendance::None,
            snapshot: None,
            tool: None,
            repeat: None,
            outstanding: None,
            outstanding_episode_broken: false,
            resolved: None,
            next_seq: 0,
            received: Vec::new(),
        }
    }

    pub fn persisted(&self) -> Persisted {
        Persisted {
            binding: self.binding.clone(),
            attendance: self.attendance.clone(),
            repeat: self.repeat.clone(),
            outstanding: self.outstanding.clone(),
            outstanding_episode_broken: self.outstanding_episode_broken,
            resolved: self.resolved.clone(),
            next_seq: self.next_seq,
            received: self.received.clone(),
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
            attendance: p.attendance,
            snapshot: None,
            tool: None,
            repeat: p.repeat,
            outstanding: None,
            outstanding_episode_broken: false,
            resolved,
            next_seq: p.next_seq,
            received,
        }
    }

    /// A new binding owns nothing of the old one's: its attendance,
    /// outstanding presentation, repeat count and received work all
    /// described another session.
    pub fn rebind(&mut self, binding: SessionId) {
        if self.binding != binding {
            *self = Self::new(binding);
        }
    }

    /// The nearest moment this state needs the monitor to act.
    pub fn next_deadline(&self) -> Option<Ms> {
        match &self.attendance {
            Attendance::Standing {
                since,
                expect,
                check_in: CheckIn::Armed,
                ..
            } => Some(since.saturating_add(*expect)),
            _ => None,
        }
    }

    /// Whether this state belongs to the facts' binding. A state for
    /// another session must be replaced, never consulted.
    pub fn bound(&self, facts: &Facts<'_>) -> bool {
        facts.binding == Some(&self.binding)
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
            Event::CheckInUndeliverable { check_in, why } => {
                self.check_in_undeliverable(&check_in, why);
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

    /// The owed work any presentation may carry now. While the attendance
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

    fn quieting(&self, facts: &Facts<'_>) -> bool {
        self.tool.is_some_and(|tool| {
            qualifies(&self.attendance, tool, self.snapshot.as_ref(), facts) == Qualifies::Yes
        })
    }

    /// Every change of attendance goes through here. Ending an attendance,
    /// or replacing it with one on other work, releases what it quieted:
    /// that work is presented again before any later attendance can quiet
    /// it. A replacement on the same work keeps it.
    fn set_attendance(&mut self, next: Attendance) {
        if self.attendance.work().is_some() && self.attendance.work() != next.work() {
            self.received.clear();
        }
        self.attendance = next;
    }

    /// The attended work is over, but not a request made while it stood:
    /// that is still to be resolved by the next report. Only an explicit
    /// clear cancels a request.
    fn end_active(&mut self) {
        let Attendance::Standing { renewal, .. } = &mut self.attendance else {
            return;
        };
        let next = renewal.take().map_or(Attendance::None, Attendance::Intent);
        self.set_attendance(next);
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
        let notice = self.resolve_intent(&report.tasks);
        self.tool = Some(report.tool);
        self.snapshot = Some(report.tasks);

        if let Some(gated) = drive_gate(facts) {
            return gated;
        }

        match qualifies(&self.attendance, report.tool, self.snapshot.as_ref(), facts) {
            Qualifies::Yes => {
                if let Attendance::Standing { check_in, .. } = &mut self.attendance
                    && *check_in == CheckIn::Unarmed
                {
                    *check_in = CheckIn::Armed;
                }
            }
            Qualifies::Ended => self.end_active(),
            Qualifies::No(_) => {}
        }

        if report.tool == Tool::Grok {
            return grok_verdict();
        }
        release_for_user_command(
            self.deliver_or_park(facts, Some(&report.place), notice),
            facts,
        )
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
            self.deliver_or_park(facts, Some(&report.place), None),
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
        self.deliver_or_park(facts, None, None)
    }

    /// The [`deliverable`](Self::deliverable) work as one new
    /// presentation, or Park when none is or another presentation is in
    /// flight. A park keeps the hook's wait armed, so work that arrives
    /// later still wakes the agent. `place` is the Stop it counts against;
    /// without one it is not counted.
    fn deliver_or_park(
        &mut self,
        facts: &Facts<'_>,
        place: Option<&TurnPlace>,
        mut notice: Option<Notice>,
    ) -> Verdict {
        let deliverable = self.deliverable(facts);
        if deliverable.is_empty() || self.outstanding.is_some() {
            return match notice.take() {
                Some(n) => Verdict::Notify(n),
                None => Verdict::Park,
            };
        }
        let p = self.present(&deliverable, place);
        let repeat = p.repeat.count();
        let verdict = Verdict::Deliver {
            presentation: p.id.clone(),
            identities: p.identities.clone(),
            repeat,
            escalated: repeat >= ESCALATE_AT,
            notice,
        };
        self.outstanding = Some(p);
        verdict
    }

    /// Resolve a pending request, an intent or a standing attendance's
    /// renewal, against THIS report's listing, which is the first that can
    /// contain work started in the turn that made it.
    fn resolve_intent(&mut self, tasks: &TaskSnapshot) -> Option<Notice> {
        let request = match &self.attendance {
            Attendance::Intent(request)
            | Attendance::Standing {
                renewal: Some(request),
                ..
            } => request.clone(),
            _ => return None,
        };
        let Request {
            selector,
            desc,
            since,
            expect,
            shell,
        } = &request;
        let live: Vec<&BackgroundTask> = tasks.live()?.collect();
        let matches: Vec<&BackgroundTask> = live
            .iter()
            .copied()
            .filter(|t| selector.matches(t))
            .collect();
        let resolved = match matches.as_slice() {
            [only] => only,
            [] => {
                let n = Notice::NoMatch {
                    selector: selector.clone(),
                    live: live.iter().copied().map(TaskSummary::from).collect(),
                };
                self.set_attendance(Attendance::None);
                return Some(n);
            }
            many => {
                let n = Notice::Ambiguous {
                    selector: selector.clone(),
                    matches: many.iter().copied().map(TaskSummary::from).collect(),
                };
                self.set_attendance(Attendance::None);
                return Some(n);
            }
        };
        let Some(task_id) = resolved.id.clone() else {
            let n = Notice::NoMatch {
                selector: selector.clone(),
                live: live.iter().copied().map(TaskSummary::from).collect(),
            };
            self.set_attendance(Attendance::None);
            return Some(n);
        };
        let kind = TaskKind::of(resolved);
        let notice = (kind == TaskKind::Shell && shell.is_none()).then(|| Notice::ShellNeedsRun {
            task_id: task_id.clone(),
        });
        let standing = Attendance::Standing {
            task_id,
            kind,
            desc: desc.clone().or_else(|| resolved.description.clone()),
            since: *since,
            expect: *expect,
            shell: shell.clone(),
            check_in: CheckIn::Unarmed,
            renewal: None,
        };
        self.set_attendance(standing);
        notice
    }

    /// Always a new presentation, counted from the last COMMITTED repeat
    /// record. A failed one is never reused: its count belongs to the
    /// episode it was made in, which an observed change may since have
    /// ended.
    fn present(&mut self, owed: &[OwedItem], place: Option<&TurnPlace>) -> Presentation {
        let identities = owed_identities(owed);
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
            AttendRequest::Attend { session, .. } | AttendRequest::Clear { session } => session,
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
            } => {
                let request = Request {
                    selector,
                    desc,
                    since: facts.now,
                    expect,
                    shell,
                };
                match &mut self.attendance {
                    Attendance::Standing { renewal, .. } => *renewal = Some(request),
                    _ => self.set_attendance(Attendance::Intent(request)),
                }
                AttendReply::Accepted
            }
            AttendRequest::Clear { .. } => {
                let had = self.attendance != Attendance::None;
                self.set_attendance(Attendance::None);
                AttendReply::Cleared { had }
            }
        }
    }

    /// Settle the in-flight presentation with its one result. A success
    /// commits its receipts and its repeat decision, which was made
    /// against the record nothing could replace while it was in flight,
    /// and is the only evidence that its work was received.
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
        for identity in p.identities {
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

    /// The one check-in, at the original deadline. Not gated on
    /// [`qualifies`]: at the deadline it is false by definition, and the
    /// check-in is exactly the wake that deadline owes. It is gated on
    /// [`wake_gate`], as every wake clank makes is, and resolves either way,
    /// so a due timer is never scheduled again.
    fn check_in_due(&mut self, facts: &Facts<'_>) -> Vec<Effect> {
        if !self.bound(facts) {
            return Vec::new();
        }
        let Attendance::Standing {
            task_id,
            desc,
            since,
            expect,
            shell,
            check_in,
            ..
        } = &mut self.attendance
        else {
            return Vec::new();
        };
        if *check_in != CheckIn::Armed || facts.now < since.saturating_add(*expect) {
            return Vec::new();
        }
        if shell.as_ref().is_some_and(|p| !facts.liveness.holds(p)) {
            self.end_active();
            return Vec::new();
        }
        if let Err(why) = wake_gate(facts) {
            *check_in = CheckIn::Withheld(why);
            return Vec::new();
        }
        *check_in = CheckIn::Fired;
        vec![Effect::CheckIn {
            check_in: CheckInRef {
                task_id: task_id.clone(),
                since: *since,
            },
            desc: desc.clone(),
            elapsed: facts.now.saturating_sub(*since),
            expect: *expect,
        }]
    }

    /// Only the attendance the check-in was fired for records its loss: a
    /// replacement made while it was being typed has its own.
    fn check_in_undeliverable(&mut self, fired: &CheckInRef, why: String) {
        if self.fired(fired)
            && let Attendance::Standing { check_in, .. } = &mut self.attendance
        {
            *check_in = CheckIn::Undeliverable(why);
        }
    }

    /// Whether the attendance is still the one `check_in` was fired for,
    /// with that check-in still in flight.
    fn fired(&self, fired: &CheckInRef) -> bool {
        matches!(&self.attendance, Attendance::Standing {
            task_id,
            since,
            check_in: CheckIn::Fired,
            ..
        } if *task_id == fired.task_id && *since == fired.since)
    }

    /// The last look before a fired check-in is typed: the session still
    /// bound, the attendance still the one it was fired for, and every
    /// wake gate still open. Anything that changed while it was prepared
    /// refuses it.
    pub fn may_type_check_in(&self, fired: &CheckInRef, facts: &Facts<'_>) -> Result<(), String> {
        if !self.bound(facts) {
            return Err("the session no longer holds the label".into());
        }
        if !self.fired(fired) {
            return Err("the attendance it was for was cleared or replaced".into());
        }
        wake_gate(facts).map_err(|why| {
            match why {
                Withheld::AutoOff => "auto turned off",
                Withheld::Asking => "a question is pending",
                Withheld::Blocked => "a block is pending",
            }
            .to_string()
        })
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
    Verdict::Notify(Notice::ArmWait)
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
    Verdict::Notify(Notice::MonitorUnavailable)
}

#[cfg(test)]
mod tests;
