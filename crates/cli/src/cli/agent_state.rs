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
    pub(crate) suppressors: Vec<Suppressor>,
    pub(crate) nudge: Nudge,
    /// How clank resumes this agent, and on what grounds, when it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resumes: Option<String>,
    /// Why clank cannot nudge this agent itself, when it cannot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cannot_resume: Option<String>,
    /// The background work the agent attends, as the monitor holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) attending: Option<Attended>,
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

/// What keeps clank from nudging, whatever the activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Suppressor {
    /// A question or block is pending, or a person is attending.
    Attending,
    /// It is parked in a wait that can take its work.
    Delivering,
    /// The harness is retrying the latest turn itself.
    NativeRetry,
    AutoOff,
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
    /// The task id it resolved to, or what it names before it has.
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
    /// The provider's own agent: its listing is its liveness, and it has
    /// no process of its own to check.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) subagent: bool,
    /// Accepted, and not yet seen in a turn-end's listing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) pending: bool,
    /// What became of its check-in, once it was due.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) check_in: Option<String>,
}

impl Attended {
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
        if self.pending {
            out.push_str(" · from the next turn-end");
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
    /// subagent, having none, never is asked for.
    pub(crate) fn marker_fields(&self, now: time::OffsetDateTime) -> Option<MarkerFields<'_>> {
        if !self.subagent && self.process_alive() != Some(true) {
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

/// `label`'s attendance as its monitor last published it, read from disk
/// now: for a decision that must not run against a lagging snapshot.
pub(crate) fn attended_now(repo: &Path, label: &str) -> Option<Attended> {
    load(&crate::agent_store::agents_root(repo).join(label))?.attending
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
}

impl View {
    /// The activity, when the state is current: a stale one claims
    /// nothing about what the agent is doing now.
    pub(crate) fn activity(&self) -> Option<&Activity> {
        self.stale.is_none().then_some(&self.state.activity)
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
    Some(View { state, stale })
}

/// The activity and its age, and what clank means to do.
fn brief(state: &AgentState, now: u64) -> String {
    let age = crate::cli::config::show_duration(now.saturating_sub(state.since));
    let mut out = match &state.activity {
        Activity::Working => format!("working {age}"),
        Activity::Idle => format!("idle {age}"),
        Activity::Failed(class) => format!("failed ({class}) {age}"),
        Activity::Asking => format!("asking {age}"),
        Activity::Interrupted => format!("interrupted {age}"),
        Activity::Unknown(_) => "unknown".to_string(),
        // Clank has nothing for it, whether a parked wait or a nudge
        // would deliver the next thing.
        Activity::Unobserved => "waiting for work".to_string(),
    };
    match &state.nudge {
        Nudge::Nothing => {}
        Nudge::At(at) => out.push_str(&format!(
            " · nudge in {}",
            crate::cli::config::show_duration(at.saturating_sub(now))
        )),
        Nudge::InFlight => out.push_str(" · nudged"),
        Nudge::WaitingForYou(_) => out.push_str(" · waiting for you"),
    }
    out
}

/// `state` in one line: [`brief`], then what holds a nudge back, why the
/// record cannot be read, why clank is waiting for a person, and why it
/// cannot resume the agent itself.
pub(crate) fn line(state: &AgentState, now: u64) -> String {
    let mut out = brief(state, now);
    if let Activity::Unknown(why) = &state.activity {
        out.push_str(&format!(": {why}"));
    }
    let unobserved = state.activity == Activity::Unobserved;
    for s in &state.suppressors {
        out.push_str(match s {
            Suppressor::Attending => " · attended",
            Suppressor::Delivering if unobserved => "",
            Suppressor::Delivering => " · waiting for work",
            Suppressor::NativeRetry => " · retrying itself",
            Suppressor::AutoOff => " · auto off",
        });
    }
    if let Nudge::WaitingForYou(why) = &state.nudge {
        out.push_str(&format!(" · {why}"));
    }
    // Nothing is owed to nudge or resume for.
    if unobserved {
        return out;
    }
    if let Some(how) = &state.resumes {
        out.push_str(&format!(" · resumed: {how}"));
    }
    if let Some(why) = &state.cannot_resume {
        out.push_str(&format!(" · not nudged: {why}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
            suppressors: Vec::new(),
            nudge: Nudge::Nothing,
            resumes: None,
            cannot_resume: None,
            attending: None,
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

    fn unobserved(suppressors: Vec<Suppressor>) -> AgentState {
        AgentState {
            activity: Activity::Unobserved,
            suppressors,
            resumes: Some("retries only".into()),
            cannot_resume: Some("its version is not known yet".into()),
            ..state()
        }
    }

    /// A parked waiter took the place of a fold, so whether work is
    /// pending is not known: it is waiting for work, said once.
    #[test]
    fn an_unobserved_agent_with_a_waiter_is_waiting_for_work() {
        let shown = line(&unobserved(vec![Suppressor::Delivering]), 1000);
        assert_eq!(shown, "waiting for work");
    }

    /// The TUI row reads the same words through `brief`, waiter or not.
    #[test]
    fn the_tui_row_reads_waiting_for_work_either_way() {
        for suppressors in [vec![Suppressor::Delivering], Vec::new()] {
            assert_eq!(brief(&unobserved(suppressors), 1000), "waiting for work");
        }
    }

    /// No parked waiter: still waiting for work, as far as the user is
    /// concerned.
    #[test]
    fn an_unobserved_agent_without_a_waiter_is_waiting_for_work_too() {
        assert_eq!(line(&unobserved(Vec::new()), 1000), "waiting for work");
        assert_eq!(
            line(&unobserved(vec![Suppressor::AutoOff]), 1000),
            "waiting for work · auto off"
        );
    }

    /// Owed work keeps every caveat.
    #[test]
    fn an_owing_agent_keeps_its_resume_and_nudge_caveats() {
        let s = AgentState {
            activity: Activity::Idle,
            suppressors: vec![Suppressor::Delivering],
            ..unobserved(Vec::new())
        };
        let shown = line(&s, 1000);
        for clause in [
            "waiting for work",
            "resumed: retries only",
            "not nudged: its version is not known yet",
        ] {
            assert!(shown.contains(clause), "{clause}: {shown}");
        }
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
            subagent: false,
            pending: false,
            check_in: None,
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

    /// A subagent has no process of its own, so the row shows it on the
    /// monitor's word; shell work is shown only while its process lives.
    #[test]
    fn a_subagent_is_drawn_without_a_process_and_shell_work_only_with_one() {
        let now = time::OffsetDateTime::now_utc();
        let mut rec = Attended {
            task: "a25eee2eef81caf3e".into(),
            expect: None,
            desc: Some("capture-probe sleeper".into()),
            pid: None,
            token: None,
            at: "2026-08-20T14:51:09Z".into(),
            subagent: true,
            pending: false,
            check_in: Some("check-in undeliverable: no pane".into()),
        };
        assert_eq!(
            rec.marker_fields(now).map(|f| f.subject),
            Some("capture-probe sleeper")
        );
        assert!(
            rec.summary(now)
                .ends_with("check-in undeliverable: no pane")
        );
        rec.subagent = false;
        assert!(rec.marker_fields(now).is_none());
    }

    /// A stale view says so and claims no activity.
    #[test]
    fn a_stale_view_claims_nothing() {
        let v = View {
            state: state(),
            stale: Some("its monitor is gone"),
        };
        assert_eq!(v.activity(), None);
        assert_eq!(v.line(1000), "stale: its monitor is gone");
        assert_eq!(v.brief(1000), "stale");
    }
}
