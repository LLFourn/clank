//! What clank knows of each agent right now, as the monitor reconciles it,
//! published for status and the TUI.
//!
//! A projection, never authority: nothing decides from a published state.
//! The monitor decides from its own fold and record reads, and writes this
//! only when it changes in meaning. A reader shows it stale once its stamps
//! no longer match the agent's binding, its role, or the living monitor.

use std::path::{Path, PathBuf};

use clank_core::Tool;

/// One agent's state, stamped with who observed it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct AgentState {
    pub(crate) role: String,
    pub(crate) session: String,
    pub(crate) tool: Tool,
    pub(crate) monitor: Observer,
    pub(crate) activity: Activity,
    /// Unix seconds: when `activity` last changed, as the monitor saw it.
    pub(crate) since: u64,
    /// What clank owes the agent and which of the four cases that puts it
    /// in. Absent when nothing is owed, and from a state published before
    /// owed work was accounted for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) owed: Option<Owing>,
    pub(crate) nudge: Nudge,
    /// Why clank cannot nudge this agent itself, when it cannot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cannot_resume: Option<String>,
    /// Every piece of background work the agent attends, as the monitor
    /// holds it.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "one_or_many"
    )]
    pub(crate) attending: Vec<Attended>,
    /// The header of the question the agent is waiting on a person to
    /// answer, while one is pending in its bound session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) asking: Option<String>,
    /// How many times in a row the same items have been delivered, once
    /// that reaches the escalation threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) repeated: Option<u32>,
}

/// The monitor that published a state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Observer {
    pub(crate) pid: i32,
    pub(crate) birth: Option<crate::proc_identity::ProcToken>,
}

impl Observer {
    pub(crate) fn me() -> Self {
        let pid = std::process::id() as i32;
        Observer {
            pid,
            birth: crate::proc_identity::token_for(pid),
        }
    }
}

/// What the agent is doing, by its record's latest turn.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "is", content = "why", rename_all = "snake_case")]
pub(crate) enum Activity {
    Working,
    /// Its wait was lost without delivering, and its record has not moved
    /// since.
    Unwatched,
    Idle,
    Failed(String),
    /// Waiting on a person to answer its question.
    Asking,
    /// Its latest turn was stopped, by whom the record does not say.
    Interrupted,
    /// It owes work and its record cannot be read.
    Unknown(String),
    /// It owes nothing, so its record is not being read. Never idle.
    Unobserved,
}

/// The work clank owes an agent, and what stands between the agent and it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Owing {
    /// The item owed longest.
    pub(crate) oldest: OwedItem,
    /// The item owed longest among those the bound session has not been
    /// delivered. `None` when every owed item reached it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) undelivered: Option<OwedItem>,
    /// How long an item may go undelivered before a row says so, in
    /// seconds: `recovery.stall_warn`, or its default while that is off.
    pub(crate) warn: u64,
    pub(crate) case: Case,
}

/// One owed item, as status shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct OwedItem {
    pub(crate) kind: String,
    pub(crate) what: String,
    pub(crate) since: crate::cli::ledger::Since,
    pub(crate) delivery: crate::cli::ledger::Delivery,
}

/// Where an agent that is owed work stands. There is no fifth case: owed
/// work is never just waiting.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "is", content = "detail", rename_all = "snake_case")]
pub(crate) enum Case {
    /// Clank must not nudge, and why.
    HandsOff(HandsOff),
    /// A presentation is with its carrier, until this unix second.
    BeingDelivered(u64),
    /// The harness's record proves the agent is going.
    Active(Active),
    /// A recovery episode is live: [`AgentState::nudge`] says what it
    /// means to do.
    Recovering,
}

/// Why clank keeps its hands off an agent it owes work.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "why", content = "detail", rename_all = "snake_case")]
pub(crate) enum HandsOff {
    AutoOff,
    /// The agent's own question to a person is pending in its session.
    Asking,
    /// The agent's own block is pending, by name.
    Blocked(String),
    /// Work it attends quiets everything it has received.
    Attending,
    /// A person stopped its latest turn.
    Interrupted,
    /// Clank has no way to resume its harness.
    CannotResume(String),
    /// A budget is spent or an attempt's fate unknown: latched for a
    /// person.
    WaitingForYou(String),
    /// Its turn is open and `recovery.stall_warn` is off.
    StallWarnOff,
    /// Its open turn went silent and `recovery.stall_nudge` is off.
    StallNudgeOff,
    /// Its record cannot be read and `recovery.blind_nudge` is off.
    BlindNudgeOff,
}

impl HandsOff {
    /// The reason in a few words, and in full.
    fn clause(&self, long: bool) -> String {
        match self {
            HandsOff::AutoOff => "auto off".into(),
            HandsOff::Asking => "asking".into(),
            HandsOff::Blocked(name) => format!("waiting on its block {name}"),
            HandsOff::Attending => "attended".into(),
            HandsOff::Interrupted => "interrupted".into(),
            HandsOff::WaitingForYou(why) if long => format!("waiting for you · {why}"),
            HandsOff::WaitingForYou(_) => "waiting for you".into(),
            HandsOff::CannotResume(why) if long => format!("not nudged: {why}"),
            HandsOff::StallWarnOff if long => {
                "not nudged: its turn is open and `recovery.stall_warn` is off".into()
            }
            HandsOff::StallNudgeOff if long => {
                "not nudged: its turn went silent and `recovery.stall_nudge` is off".into()
            }
            HandsOff::BlindNudgeOff if long => {
                "not nudged: its record cannot be read and `recovery.blind_nudge` is off".into()
            }
            HandsOff::CannotResume(_)
            | HandsOff::StallWarnOff
            | HandsOff::StallNudgeOff
            | HandsOff::BlindNudgeOff => "not nudged".into(),
        }
    }
}

/// What proves an agent is going without clank.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Active {
    /// Its latest turn has not ended, and its record moved inside the
    /// stall window.
    Turn,
    /// The harness is retrying the latest turn itself.
    NativeRetry,
}

/// What clank means to do about the agent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "is", content = "detail", rename_all = "snake_case")]
pub(crate) enum Nudge {
    Nothing,
    /// The next look, at this unix second.
    At(u64),
    /// A nudge or retry is out, and its turn not yet seen to finish.
    InFlight,
    /// Its running turn has been silent since this unix second; the nudge
    /// is due `at`, or never.
    Silent {
        since: u64,
        at: Option<u64>,
    },
    /// Clank has stopped: the reason its pending work waits for a person.
    WaitingForYou(String),
}

pub(crate) fn path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("state.json")
}

/// How long attended work was expected to take, from the moment its
/// attendance was accepted.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Expectation {
    /// RFC3339.
    pub(crate) since: String,
    pub(crate) secs: u64,
}

impl Expectation {
    fn since(&self) -> Option<time::OffsetDateTime> {
        time::OffsetDateTime::parse(&self.since, &time::format_description::well_known::Rfc3339)
            .ok()
    }

    fn elapsed(&self, now: time::OffsetDateTime) -> time::Duration {
        now - self.since().unwrap_or(now)
    }

    fn short(&self) -> String {
        short_duration(time::Duration::seconds(self.secs as i64))
    }
}

/// An attendance, as status and the TUI show it.
///
/// A projection of the monitor's state and never authority: an
/// inaccurate one misinforms a status line, and nothing quiets an agent
/// on it.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attended {
    /// What `clank attend` names it by: a declaration's name, or a run's
    /// `--desc`.
    pub(crate) task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expect: Option<Expectation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) desc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pid: Option<i32>,
    /// WHO the pid was when it was recorded: a pid alone is reused, so
    /// anything that would signal this process compares the token first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) token: Option<crate::proc_identity::ProcToken>,
    /// RFC3339, when the attendance was accepted.
    pub(crate) at: String,
    /// A `clank attend` declaration: it has no process of its own to
    /// check.
    #[serde(
        default,
        alias = "subagent",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub(crate) declared: bool,
    /// What became of its check-in, once it was due.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) check_in: Option<String>,
    /// Why it covers nothing now, while it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) unqualified: Option<String>,
}

/// A state a monitor published before an agent could attend several
/// things carries one attendance, not a list.
fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Attended>, D::Error> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Shape {
        Many(Vec<Attended>),
        One(Box<Attended>),
    }
    Ok(
        match <Option<Shape> as serde::Deserialize>::deserialize(d)? {
            Some(Shape::Many(all)) => all,
            Some(Shape::One(one)) => vec![*one],
            None => Vec::new(),
        },
    )
}

impl Attended {
    /// Whether it covers its work now: the monitor said so, and a run's
    /// process is positively still there. A declaration has no process.
    pub(crate) fn covers(&self) -> bool {
        self.unqualified.is_none() && (self.declared || self.process_alive() == Some(true))
    }

    /// One line for a detail page: what it attends, and its age against
    /// its bound while it covers its work, else why it does not.
    pub(crate) fn line(&self, now: time::OffsetDateTime) -> String {
        match (&self.unqualified, self.covers()) {
            (Some(why), _) => format!("○ {} — {why}", self.subject()),
            (None, true) => match self.age(now) {
                Some(age) => format!("⌛ {} · {age}", self.subject()),
                None => format!("⌛ {}", self.subject()),
            },
            (None, false) => format!("○ {} — process ended", self.subject()),
        }
    }

    /// Whether the recorded process still exists, or `None` when there is
    /// no pid to ask about.
    pub(crate) fn process_alive(&self) -> Option<bool> {
        self.pid.map(crate::signal::alive)
    }

    /// How the attendance reads on a status surface: the pid a human can
    /// find in `ps` when there is one, the handle that names it, and
    /// either how long it has run against its bound or the fact that its
    /// process has since ended.
    pub(crate) fn summary(&self, now: time::OffsetDateTime) -> String {
        let mut out = match self.pid {
            Some(pid) => format!("{pid} ({})", self.task),
            None => self.task.clone(),
        };
        if let Some(why) = &self.unqualified {
            out.push_str(" · ");
            out.push_str(why);
        }
        match (self.process_alive(), self.age(now)) {
            (Some(false), _) => out.push_str(" · stale, ended"),
            (_, Some(age)) => {
                out.push_str(" · ");
                out.push_str(&age);
            }
            _ => {}
        }
        if let Some(check_in) = &self.check_in {
            out.push_str(" · ");
            out.push_str(check_in);
        }
        out
    }

    /// What the wait IS: the description, else the task. Never empty.
    pub(crate) fn subject(&self) -> &str {
        self.desc
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .unwrap_or(&self.task)
    }

    /// The pid this wait may be signalled at, or `None` when the process
    /// cannot be identified: a reused pid can belong to a stranger.
    pub(crate) fn killable_pid(&self) -> Option<crate::signal::Pid> {
        let pid = crate::signal::Pid::new(self.pid?)?;
        let token = self.token.as_ref()?;
        token.still_holds(pid.get()).then_some(pid)
    }

    /// Why this wait cannot be killed, phrased for the page. `None` when
    /// it can be.
    pub(crate) fn kill_blocker(&self) -> Option<&'static str> {
        let Some(pid) = self.pid else {
            return Some("no pid was recorded for this wait, so there is nothing to signal");
        };
        let Some(token) = self.token.as_ref() else {
            return Some(
                "this wait was recorded before clank identified processes, \
                 so the pid cannot be trusted to still be the same process",
            );
        };
        if !crate::signal::alive(pid) {
            return Some("that process has already ended");
        }
        (!token.still_holds(pid)).then_some(
            "the pid is in use by a DIFFERENT process now — the recorded one has ended \
             and its number was reused",
        )
    }

    /// The row marker's fields in priority order: subject, then age, then
    /// pid. Callers drop from the RIGHT to fit. `None` for a wait the row
    /// cannot show as ongoing: a process not positively alive, which a
    /// declaration, having none, never is asked for.
    pub(crate) fn marker_fields(&self, now: time::OffsetDateTime) -> Option<MarkerFields<'_>> {
        if !self.covers() {
            return None;
        }
        Some(MarkerFields {
            subject: self.subject(),
            age: self.age(now),
            pid: self.pid,
        })
    }

    /// The work's age against its bound — `3m/5m`, and `12m/5m` once
    /// overdue. Without an expectation it ages from `at`.
    pub(crate) fn age(&self, now: time::OffsetDateTime) -> Option<String> {
        let Some(expect) = self.expect.as_ref() else {
            let at = time::OffsetDateTime::parse(
                &self.at,
                &time::format_description::well_known::Rfc3339,
            )
            .ok()?;
            return Some(short_duration(now - at));
        };
        Some(format!(
            "{}/{}",
            short_duration(expect.elapsed(now)),
            expect.short()
        ))
    }
}

/// The attendance marker's parts, most important first. Assembling
/// and fitting them belongs to the renderer.
pub(crate) struct MarkerFields<'a> {
    pub(crate) subject: &'a str,
    pub(crate) age: Option<String>,
    pub(crate) pid: Option<i32>,
}

/// An expectation is a BUDGET (`45s/5m`), so seconds are kept.
fn short_duration(d: time::Duration) -> String {
    crate::age::short(d.whole_seconds())
}

/// `label`'s attendances as its monitor last published them, read from
/// disk now: for a decision that must not run against a lagging snapshot.
pub(crate) fn attended_now(repo: &Path, label: &str) -> Vec<Attended> {
    load(&crate::agent_store::agents_root(repo).join(label))
        .map(|s| s.attending)
        .unwrap_or_default()
}

pub(crate) fn load(agent_dir: &Path) -> Option<AgentState> {
    serde_json::from_slice(&std::fs::read(path(agent_dir)).ok()?).ok()
}

/// Publish `state`. Best effort: a failed write changes nothing else.
pub(crate) fn store(agent_dir: &Path, state: &AgentState) {
    let path = path(agent_dir);
    let tmp = path.with_extension("json.tmp");
    if let Ok(bytes) = serde_json::to_vec_pretty(state) {
        let _ = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, &path));
    }
}

/// Why `state` no longer describes the agent, if it does not: another
/// session is bound, the roster gives another role, or the monitor that
/// published it is not the living, published one.
pub(crate) fn stale(
    state: &AgentState,
    session: Option<&str>,
    role: &str,
    monitor: Option<i32>,
    living: impl Fn(&Observer) -> bool,
) -> Option<&'static str> {
    if session != Some(state.session.as_str()) {
        Some("another session is bound")
    } else if role != state.role {
        Some("its role changed")
    } else if monitor != Some(state.monitor.pid) || !living(&state.monitor) {
        Some("its monitor is gone")
    } else {
        None
    }
}

/// A published state as a reader sees it: the state itself, and why it no
/// longer describes the agent when it does not.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct View {
    pub(crate) state: AgentState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stale: Option<&'static str>,
    /// The oldest item a current state says is owed, aged when the view
    /// was taken.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) owed: Option<OwedView>,
}

impl View {
    /// The activity, when the state is current: a stale one claims
    /// nothing about what the agent is doing now.
    pub(crate) fn activity(&self) -> Option<&Activity> {
        self.stale.is_none().then_some(&self.state.activity)
    }

    /// Whether the agent's latest turn is currently known to have been
    /// interrupted. A stale state says nothing about now, so it is never.
    pub(crate) fn interrupted(&self) -> bool {
        self.activity() == Some(&Activity::Interrupted)
    }

    /// The owed item the bound session has gone without for longer than
    /// the warn threshold, as a row shouts it. A stale state shouts
    /// nothing.
    pub(crate) fn undelivered(&self, now: u64) -> Option<String> {
        self.stale
            .is_none()
            .then(|| undelivered(&self.state, now))
            .flatten()
    }

    /// Whether a row may say the agent is working: its record shows a
    /// turn going, and nothing owed to it says otherwise.
    pub(crate) fn working(&self) -> bool {
        self.activity() == Some(&Activity::Working) && proven_working(&self.state)
    }

    /// `state` as a reader at `now` sees it. A stale state says nothing
    /// is owed: what it knew is not known now.
    pub(crate) fn at(state: AgentState, stale: Option<&'static str>, now: u64) -> Self {
        let owed = state
            .owed
            .as_ref()
            .filter(|_| stale.is_none())
            .map(|owing| OwedView {
                age: owing.oldest.since.known().map(|at| now.saturating_sub(at)),
                kind: owing.oldest.kind.clone(),
                what: owing.oldest.what.clone(),
                delivery: owing.oldest.delivery,
            });
        View { state, stale, owed }
    }

    /// The whole state in one line, with every reason it gives.
    pub(crate) fn line(&self, now: u64) -> String {
        match self.stale {
            Some(why) => format!("stale: {why}"),
            None => line(&self.state, now),
        }
    }

    /// The state in a few words, for a row with no room for reasons.
    pub(crate) fn brief(&self, now: u64) -> String {
        match self.stale {
            Some(_) => "stale".to_string(),
            None => brief(&self.state, now),
        }
    }

    /// [`Self::brief`] for a roster row: an agent waiting for work is the
    /// resting state and says nothing, keeping only its clauses.
    pub(crate) fn row(&self, now: u64) -> Option<String> {
        match self.activity() {
            Some(Activity::Unobserved) => {
                let clauses = clauses(&self.state, now, false, false);
                (!clauses.is_empty()).then(|| clauses.join(" · "))
            }
            _ => Some(self.brief(now)),
        }
    }
}

/// `label`'s published state, judged against its binding (`session`), its
/// roster role, and the living, published monitor.
pub(crate) fn view(
    repo: &Path,
    label: &clank_core::ids::AgentLabel,
    session: Option<&str>,
) -> Option<View> {
    let state = load(&crate::agent_store::agents_root(repo).join(label.as_str()))?;
    let role = crate::agent_store::resolve_role(repo, label)
        .map(|r| r.to_string())
        .unwrap_or_default();
    let monitor = match crate::cli::monitor::published(repo) {
        crate::cli::monitor::Published::Ours(d) => Some(d.pid),
        _ => None,
    };
    let living = |o: &Observer| o.birth.as_ref().is_some_and(|b| b.still_holds(o.pid));
    let stale = stale(&state, session, &role, monitor, living);
    Some(View::at(state, stale, crate::cli::recovery::unix_now()))
}

/// The oldest owed item as `clank status -j` carries it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct OwedView {
    /// Seconds since it became owed; `null` when no record dates it.
    pub(crate) age: Option<u64>,
    pub(crate) kind: String,
    pub(crate) what: String,
    pub(crate) delivery: crate::cli::ledger::Delivery,
}

/// How owed work the bound session never received is named wherever a
/// person reads an agent's state.
pub(crate) const UNDELIVERED: &str = "UNDELIVERED";

/// The SGR undelivered work is drawn in: bold bright red.
pub(crate) const UNDELIVERED_SGR: &str = "1;91";

/// `UNDELIVERED <age> · <what>` once the oldest item the bound session has
/// not received has gone without for the warn threshold. One in flight
/// inside its bound is being delivered, not missing. An undated item is
/// timed from when the ledger first held it, and its age is said to be
/// unknown.
fn undelivered(state: &AgentState, now: u64) -> Option<String> {
    use crate::cli::ledger::{Delivery, Since};
    let owing = state.owed.as_ref()?;
    let item = owing.undelivered.as_ref()?;
    if matches!(item.delivery, Delivery::InFlight(until) if now < until)
        || now < item.since.no_later_than().saturating_add(owing.warn)
    {
        return None;
    }
    let age = match item.since {
        Since::At(at) => crate::age::duration(now.saturating_sub(at)),
        Since::Unknown { .. } => "age unknown".to_string(),
    };
    Some(format!("{UNDELIVERED} {age} · {}", item.what))
}

/// Whether `working` is something the state proves: nothing is owed, or
/// the owed work's case is the record's own evidence of a turn going.
fn proven_working(state: &AgentState) -> bool {
    state
        .owed
        .as_ref()
        .is_none_or(|o| matches!(o.case, Case::Active(_)))
}

/// How an interrupted agent is named wherever a person reads its state.
pub(crate) const INTERRUPTED: &str = "INTERRUPTED";

/// The SGR an interrupted agent is drawn in: bold orange #d75f00, 3.8:1
/// on a white background and 5.5:1 on black. The head-correction orange
/// (208, #ff8700) is 2.4:1 on white.
pub(crate) const INTERRUPTED_SGR: &str = "1;38;5;166";

/// The activity and its age, and what clank means to do.
fn brief(state: &AgentState, now: u64) -> String {
    said(state, now, false)
}

fn said(state: &AgentState, now: u64, long: bool) -> String {
    let shouted = undelivered(state, now);
    let mut parts = vec![shouted.clone().unwrap_or_else(|| doing(state, now))];
    parts.extend(clauses(state, now, long, shouted.is_some()));
    parts.join(" · ")
}

/// What the agent is doing, by its record, and for how long.
fn doing(state: &AgentState, now: u64) -> String {
    let age = crate::age::duration(now.saturating_sub(state.since));
    match &state.activity {
        Activity::Working if proven_working(state) => format!("working {age}"),
        // Its turn has not ended, and nothing shows it moving.
        Activity::Working => format!("turn open {age}"),
        Activity::Unwatched => format!("unwatched {age}"),
        Activity::Idle => format!("idle {age}"),
        Activity::Failed(class) => format!("failed ({class}) {age}"),
        Activity::Asking => format!("asking {age}"),
        // Recovery leaves an interrupted turn alone, so a person reading
        // this is the only one who will act on it.
        Activity::Interrupted => format!("{INTERRUPTED} {age}"),
        Activity::Unknown(_) => "unknown".to_string(),
        // Clank has nothing for it, whether a parked wait or a nudge
        // would deliver the next thing.
        Activity::Unobserved => "waiting for work".to_string(),
    }
}

/// What stands between the agent and its owed work, then what clank
/// means to do. `shouted` when the row leads with the undelivered item
/// rather than the activity, which then no longer says the reason itself.
fn clauses(state: &AgentState, now: u64, long: bool, shouted: bool) -> Vec<String> {
    let mut out = Vec::new();
    match state.owed.as_ref().map(|o| &o.case) {
        Some(Case::HandsOff(why)) => {
            let headline_says = match why {
                HandsOff::Interrupted => state.activity == Activity::Interrupted,
                HandsOff::Asking => state.activity == Activity::Asking,
                _ => false,
            };
            // The nudge's own clause says it, and why.
            let nudge_says = matches!(why, HandsOff::WaitingForYou(_))
                && matches!(state.nudge, Nudge::WaitingForYou(_));
            if (shouted || !headline_says) && !nudge_says {
                out.push(why.clause(long));
            }
        }
        Some(Case::BeingDelivered(_)) => out.push("being delivered".to_string()),
        Some(Case::Active(Active::NativeRetry)) => out.push("retrying itself".to_string()),
        Some(Case::Active(Active::Turn) | Case::Recovering) | None => {}
    }
    // A nudge clank will not send is not promised beside the reason it
    // will not.
    let held = matches!(
        state.owed.as_ref().map(|o| &o.case),
        Some(Case::HandsOff(_))
    );
    if !held || matches!(state.nudge, Nudge::WaitingForYou(_)) {
        out.extend(nudge_clauses(state, now));
    }
    if long && let Nudge::WaitingForYou(why) = &state.nudge {
        out.push(why.clone());
    }
    out
}

/// What clank means to do about the agent.
fn nudge_clauses(state: &AgentState, now: u64) -> Vec<String> {
    let show = crate::age::duration;
    match &state.nudge {
        Nudge::Nothing => Vec::new(),
        Nudge::At(at) => vec![format!("nudge in {}", show(at.saturating_sub(now)))],
        Nudge::InFlight => vec!["nudged".to_string()],
        Nudge::Silent { since, at } => {
            let mut clause = format!("silent {}", show(now.saturating_sub(*since)));
            if let Some(at) = at {
                clause.push_str(&format!(" — nudging in {}", show(at.saturating_sub(now))));
            }
            vec![clause]
        }
        Nudge::WaitingForYou(_) => vec!["waiting for you".to_string()],
    }
}

/// `state` in one line: [`brief`] with every reason in full, then why the
/// record cannot be read and why clank cannot resume the agent itself.
pub(crate) fn line(state: &AgentState, now: u64) -> String {
    let mut out = said(state, now, true);
    if let Activity::Unknown(why) = &state.activity {
        out.push_str(&format!(": {why}"));
    }
    // Nothing is owed to nudge or resume for.
    if state.activity == Activity::Unobserved {
        return out;
    }
    let said_already = matches!(
        state.owed.as_ref().map(|o| &o.case),
        Some(Case::HandsOff(HandsOff::CannotResume(_)))
    );
    if let Some(why) = state.cannot_resume.as_ref().filter(|_| !said_already) {
        out.push_str(&format!(" · not nudged: {why}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ledger::{Delivery, Since};

    fn state() -> AgentState {
        AgentState {
            role: "master".into(),
            session: "s1".into(),
            tool: Tool::Claude,
            monitor: Observer {
                pid: 42,
                birth: None,
            },
            activity: Activity::Idle,
            since: 100,
            owed: None,
            nudge: Nudge::Nothing,
            cannot_resume: None,
            attending: Vec::new(),
            asking: None,
            repeated: None,
        }
    }

    /// Each stamp that no longer matches makes the state stale; all of
    /// them matching does not.
    #[test]
    fn a_state_whose_stamps_moved_on_is_stale() {
        let s = state();
        let alive = |_: &Observer| true;
        assert_eq!(stale(&s, Some("s1"), "master", Some(42), alive), None);
        assert!(stale(&s, Some("s2"), "master", Some(42), alive).is_some());
        assert!(stale(&s, None, "master", Some(42), alive).is_some());
        assert!(stale(&s, Some("s1"), "commit", Some(42), alive).is_some());
        assert!(stale(&s, Some("s1"), "master", Some(43), alive).is_some());
        assert!(stale(&s, Some("s1"), "master", None, alive).is_some());
        assert!(stale(&s, Some("s1"), "master", Some(42), |_| false).is_some());
    }

    fn unobserved() -> AgentState {
        AgentState {
            activity: Activity::Unobserved,
            cannot_resume: Some("no pane named `claude` in /r's tab".into()),
            ..state()
        }
    }

    /// An agent owed nothing is waiting for work, said once, with no
    /// caveat about a nudge nothing calls for.
    #[test]
    fn an_agent_owed_nothing_is_waiting_for_work() {
        assert_eq!(line(&unobserved(), 1000), "waiting for work");
        assert_eq!(brief(&unobserved(), 1000), "waiting for work");
    }

    /// An age is read by a person, so 20 hours reads `20h`, not the second
    /// count the exact config formatter would print.
    #[test]
    fn ages_and_countdowns_read_as_times() {
        let interrupted = AgentState {
            activity: Activity::Interrupted,
            since: 100,
            ..state()
        };
        assert_eq!(brief(&interrupted, 100 + 72_090), "INTERRUPTED 20h");
        let due = AgentState {
            nudge: Nudge::At(1000 + 150),
            ..state()
        };
        assert!(
            brief(&due, 1000).ends_with(" · nudge in 2m"),
            "{}",
            brief(&due, 1000)
        );
        let future = AgentState {
            since: 2000,
            ..state()
        };
        assert_eq!(brief(&future, 1000), "idle 0s");
        assert_eq!(crate::cli::config::show_duration(90), "90s");
    }

    /// A roster row leaves the resting state unsaid and keeps its clauses,
    /// while `line` still says it; every other activity reads as `brief`.
    #[test]
    fn an_idle_roster_row_says_nothing_but_status_still_does() {
        let waiting = View {
            owed: None,
            state: unobserved(),
            stale: None,
        };
        assert_eq!(waiting.row(1000), None);
        assert_eq!(waiting.line(1000), "waiting for work");
        let nudging = View {
            owed: None,
            state: AgentState {
                nudge: Nudge::InFlight,
                ..unobserved()
            },
            stale: None,
        };
        assert_eq!(nudging.row(1000).as_deref(), Some("nudged"));
        for activity in [Activity::Working, Activity::Idle, Activity::Interrupted] {
            let v = View {
                owed: None,
                state: AgentState {
                    activity,
                    ..state()
                },
                stale: None,
            };
            assert_eq!(v.row(1000), Some(v.brief(1000)));
        }
    }

    fn owing(item: OwedItem, case: Case) -> Owing {
        Owing {
            undelivered: (item.delivery != Delivery::Delivered).then(|| item.clone()),
            oldest: item,
            warn: 2400,
            case,
        }
    }

    fn message(since: Since, delivery: Delivery) -> OwedItem {
        OwedItem {
            kind: "message".into(),
            what: "message from claude".into(),
            since,
            delivery,
        }
    }

    fn view(state: AgentState) -> View {
        View::at(state, None, 0)
    }

    /// An item the bound session has not received reads `UNDELIVERED` once
    /// it has gone without for the warn threshold, and not before; one in
    /// flight inside its bound is being delivered; one delivered and still
    /// owed reads as its recovery state.
    #[test]
    fn undelivered_work_is_shouted_only_past_the_threshold() {
        let owed = |delivery, case| AgentState {
            owed: Some(owing(message(Since::At(1000), delivery), case)),
            nudge: Nudge::At(1000 + 7200 + 60),
            ..state()
        };
        let missing = owed(Delivery::NotDelivered, Case::Recovering);
        assert_eq!(view(missing.clone()).undelivered(1000 + 2399), None);
        assert_eq!(
            brief(&missing, 1000 + 7200),
            "UNDELIVERED 2h · message from claude · nudge in 1m"
        );
        let flying = owed(Delivery::InFlight(1000 + 7260), Case::BeingDelivered(8260));
        assert_eq!(view(flying.clone()).undelivered(1000 + 7200), None);
        assert!(
            view(flying).undelivered(1000 + 7260).is_some(),
            "past its bound it is not on its way"
        );
        let delivered = owed(Delivery::Delivered, Case::Recovering);
        assert_eq!(view(delivered.clone()).undelivered(1000 + 7200), None);
        assert_eq!(brief(&delivered, 1000 + 7200), "idle 2h · nudge in 1m");
    }

    /// An item no record dates is timed from when the ledger first held
    /// it, and its age is said to be unknown, never counted from there.
    #[test]
    fn an_undated_item_says_its_age_is_unknown() {
        let s = AgentState {
            owed: Some(owing(
                message(Since::Unknown { held: 5000 }, Delivery::NotDelivered),
                Case::Recovering,
            )),
            ..state()
        };
        assert_eq!(view(s.clone()).undelivered(5000 + 2399), None);
        assert_eq!(
            view(s.clone()).undelivered(5000 + 2400).as_deref(),
            Some("UNDELIVERED age unknown · message from claude")
        );
        assert_eq!(View::at(s, None, 9000).owed.unwrap().age, None);
    }

    /// Every reason clank keeps its hands off is said in the row, and in
    /// full in the line; an undelivered row says it after what is missing.
    #[test]
    fn each_hands_off_reason_is_shown() {
        for (why, short, long) in [
            (HandsOff::AutoOff, "auto off", "auto off"),
            (
                HandsOff::Blocked("stork".into()),
                "waiting on its block stork",
                "waiting on its block stork",
            ),
            (HandsOff::Attending, "attended", "attended"),
            (
                HandsOff::CannotResume("no way to resume it".into()),
                "not nudged",
                "not nudged: no way to resume it",
            ),
            (
                HandsOff::StallWarnOff,
                "not nudged",
                "not nudged: its turn is open and `recovery.stall_warn` is off",
            ),
            (
                HandsOff::StallNudgeOff,
                "not nudged",
                "not nudged: its turn went silent and `recovery.stall_nudge` is off",
            ),
            (
                HandsOff::BlindNudgeOff,
                "not nudged",
                "not nudged: its record cannot be read and `recovery.blind_nudge` is off",
            ),
            (
                HandsOff::WaitingForYou("budget spent".into()),
                "waiting for you",
                "waiting for you · budget spent",
            ),
        ] {
            let s = AgentState {
                owed: Some(owing(
                    message(Since::At(100), Delivery::Delivered),
                    Case::HandsOff(why.clone()),
                )),
                ..state()
            };
            assert_eq!(brief(&s, 1000), format!("idle 15m · {short}"), "{why:?}");
            assert_eq!(line(&s, 1000), format!("idle 15m · {long}"), "{why:?}");
        }
        let said_by_the_headline = |activity, why| AgentState {
            activity,
            owed: Some(owing(
                message(Since::At(100), Delivery::Delivered),
                Case::HandsOff(why),
            )),
            ..state()
        };
        assert_eq!(
            brief(
                &said_by_the_headline(Activity::Interrupted, HandsOff::Interrupted),
                1000
            ),
            "INTERRUPTED 15m"
        );
        assert_eq!(
            brief(
                &said_by_the_headline(Activity::Asking, HandsOff::Asking),
                1000
            ),
            "asking 15m"
        );
        let undelivered = AgentState {
            activity: Activity::Interrupted,
            owed: Some(owing(
                message(Since::At(100), Delivery::NotDelivered),
                Case::HandsOff(HandsOff::Interrupted),
            )),
            ..state()
        };
        assert_eq!(
            brief(&undelivered, 100 + 7200),
            "UNDELIVERED 2h · message from claude · interrupted"
        );
    }

    /// A row says `working` only on the record's own evidence: an open
    /// turn clank is recovering, delivering to or keeping its hands off is
    /// a turn that is open, and nothing more.
    #[test]
    fn working_is_said_only_on_evidence() {
        let running = |case: Option<Case>| AgentState {
            activity: Activity::Working,
            owed: case.map(|case| owing(message(Since::At(100), Delivery::Delivered), case)),
            ..state()
        };
        for proven in [
            None,
            Some(Case::Active(Active::Turn)),
            Some(Case::Active(Active::NativeRetry)),
        ] {
            let s = running(proven.clone());
            assert!(view(s.clone()).working(), "{proven:?}");
            assert!(brief(&s, 1000).starts_with("working 15m"), "{proven:?}");
        }
        assert_eq!(
            brief(&running(Some(Case::Active(Active::NativeRetry))), 1000),
            "working 15m · retrying itself"
        );
        for unproven in [
            Case::HandsOff(HandsOff::AutoOff),
            Case::BeingDelivered(2000),
            Case::Recovering,
        ] {
            let s = running(Some(unproven.clone()));
            assert!(!view(s.clone()).working(), "{unproven:?}");
            assert!(brief(&s, 1000).starts_with("turn open 15m"), "{unproven:?}");
        }
        assert_eq!(
            brief(&running(Some(Case::BeingDelivered(2000))), 1000),
            "turn open 15m · being delivered"
        );
    }

    /// A stale view shouts nothing and reports nothing owed.
    #[test]
    fn a_stale_view_has_no_owed_work_to_show() {
        let s = AgentState {
            owed: Some(owing(
                message(Since::At(0), Delivery::NotDelivered),
                Case::Recovering,
            )),
            ..state()
        };
        let stale = View::at(s.clone(), Some("its monitor is gone"), 9000);
        assert_eq!(stale.undelivered(9000), None);
        assert_eq!(stale.owed, None);
        let current = View::at(s, None, 9000);
        assert_eq!(
            current.owed,
            Some(OwedView {
                age: Some(9000),
                kind: "message".into(),
                what: "message from claude".into(),
                delivery: Delivery::NotDelivered,
            })
        );
    }

    /// A state published before owed work was accounted for, with the
    /// suppressor list it carried, still loads and says nothing is owed.
    #[test]
    fn a_state_published_before_the_ledger_still_loads() {
        let old: AgentState =
            serde_json::from_str(include_str!("../../tests/fixtures/formats/state.json")).unwrap();
        assert_eq!(old.owed, None);
        assert_eq!(old.activity, Activity::Unobserved);
        assert_eq!(old.attending.len(), 1);
        let new: AgentState = serde_json::from_str(include_str!(
            "../../tests/fixtures/formats/state-owing.json"
        ))
        .unwrap();
        let owed = new.owed.unwrap();
        assert_eq!(owed.case, Case::HandsOff(HandsOff::Blocked("stork".into())));
        assert_eq!(owed.oldest.since, Since::At(1793845000));
        assert_eq!(
            owed.undelivered.unwrap().delivery,
            Delivery::InFlight(1793845260)
        );
    }

    /// The line gives every reason the state holds: why the record cannot
    /// be read, why clank waits for a person, and why it cannot resume.
    #[test]
    fn the_line_explains_itself() {
        let s = AgentState {
            activity: Activity::Unknown("its record could not be read".into()),
            nudge: Nudge::WaitingForYou("retrying did not recover it".into()),
            cannot_resume: Some("no evidence".into()),
            ..state()
        };
        let shown = line(&s, 1000);
        for why in [
            "its record could not be read",
            "retrying did not recover it",
            "no evidence",
        ] {
            assert!(shown.contains(why), "{shown}");
        }
        assert!(
            !brief(&s, 1000).contains("no evidence"),
            "a brief has no reasons"
        );
    }

    /// Records written before a description existed keep loading, with
    /// the task as the subject; a blank description is not one.
    #[test]
    fn an_attended_record_without_a_description_still_loads_and_names_itself() {
        let raw = r#"{"task":"br9711ewy","at":"2026-08-20T14:51:09Z"}"#;
        let rec: Attended = serde_json::from_str(raw).expect("pre-desc record still parses");
        assert_eq!(rec.desc, None);
        assert_eq!(rec.subject(), "br9711ewy", "the id is the subject");
        let blank = r#"{"task":"br9711ewy","desc":"   ","at":"2026-08-20T14:51:09Z"}"#;
        let rec: Attended = serde_json::from_str(blank).unwrap();
        assert_eq!(rec.subject(), "br9711ewy", "whitespace never names a wait");
        let full = r#"{"task":"br9711ewy","desc":"test run","at":"2026-08-20T14:51:09Z"}"#;
        let rec: Attended = serde_json::from_str(full).unwrap();
        assert_eq!(rec.subject(), "test run");
        let rec: Attended =
            serde_json::from_str(r#"{"task":"t","pid":1,"at":"2026-08-20T14:51:09Z"}"#).unwrap();
        assert!(rec.expect.is_none());
    }

    #[test]
    fn the_projection_ages_the_work_against_its_bound() {
        let now = time::OffsetDateTime::now_utc();
        let at = |secs_ago: i64| {
            (now - time::Duration::seconds(secs_ago))
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        };
        let mut rec = Attended {
            task: "t".into(),
            expect: Some(Expectation {
                since: at(180),
                secs: 300,
            }),
            desc: None,
            pid: None,
            token: None,
            at: at(10),
            declared: false,
            check_in: None,
            unqualified: None,
        };
        assert_eq!(rec.age(now).as_deref(), Some("3m/5m"));
        rec.expect.as_mut().unwrap().since = at(720);
        assert_eq!(
            rec.age(now).as_deref(),
            Some("12m/5m"),
            "overdue says so by itself"
        );
        rec.expect = None;
        assert_eq!(
            rec.age(now).as_deref(),
            Some("10s"),
            "without one: from `at`"
        );
    }

    /// A declaration has no process of its own, so the row shows it on the
    /// monitor's word; shell work is shown only while its process lives.
    #[test]
    fn a_declaration_is_drawn_without_a_process_and_shell_work_only_with_one() {
        let now = time::OffsetDateTime::now_utc();
        let mut rec = Attended {
            task: "a25eee2eef81caf3e".into(),
            expect: None,
            desc: Some("capture-probe sleeper".into()),
            pid: None,
            token: None,
            at: "2026-08-20T14:51:09Z".into(),
            declared: true,
            check_in: Some("check-in undeliverable: no pane".into()),
            unqualified: None,
        };
        assert_eq!(
            rec.marker_fields(now).map(|f| f.subject),
            Some("capture-probe sleeper")
        );
        assert!(
            rec.summary(now)
                .ends_with("check-in undeliverable: no pane")
        );
        rec.declared = false;
        assert!(rec.marker_fields(now).is_none());
    }

    /// A stale view says so and claims no activity, even one whose
    /// record says it was interrupted.
    #[test]
    fn a_stale_view_claims_nothing() {
        let interrupted = AgentState {
            activity: Activity::Interrupted,
            ..state()
        };
        let current = View {
            owed: None,
            state: interrupted.clone(),
            stale: None,
        };
        assert!(current.interrupted());
        let v = View {
            owed: None,
            state: interrupted,
            stale: Some("its monitor is gone"),
        };
        assert_eq!(v.activity(), None);
        assert!(!v.interrupted());
        assert_eq!(v.line(1000), "stale: its monitor is gone");
        assert_eq!(v.brief(1000), "stale");
    }
}
