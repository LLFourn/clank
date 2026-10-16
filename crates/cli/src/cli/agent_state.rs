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
