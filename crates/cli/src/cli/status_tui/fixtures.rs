//! Shared test fixtures for the `status_tui` modules — snapshot/agent/PR
//! builders and the ANSI-stripping assertion helpers. Lives in its own
//! module (not the IO-shell `mod.rs`) so every sibling module's tests
//! import the same builders from one neutral place.

use crate::cli::status::StatusSnapshot;
use crate::lifecycle::{AgentLabel, CommitSha, PlanKey};
use clank_core::plan_view::WaitingOn;
use clank_core::repo_state::NonEmptyVec;
use clank_core::vocab::CommitGateState;
use clank_core::wait::PlanWorkState;

pub(crate) fn agent_row(
    label: &str,
    role: crate::cli::teams_config::RosterRole,
    auto: clank_core::vocab::AutoMode,
) -> crate::cli::status::AgentAutoRow {
    crate::cli::status::AgentAutoRow {
        label: label.to_string(),
        role,
        auto_mode: auto,
        tool: "claude".to_string(),
        invocation: "claude".to_string(),
        session: None,
        attending: None,
    }
}

pub(crate) fn two_agent_snap() -> StatusSnapshot {
    use crate::cli::teams_config::RosterRole;
    use clank_core::vocab::AutoMode;
    let mut s = snap(vec![], vec![]);
    s.agents = vec![
        agent_row("claude", RosterRole::Master, AutoMode::On),
        agent_row("codex", RosterRole::Commit, AutoMode::Off),
    ];
    s
}

/// The candidate list the picker renders, with invocation + desc.
pub(crate) fn cand(
    label: &str,
    tool: &str,
    invocation: &str,
) -> crate::cli::status::AvailableAgent {
    crate::cli::status::AvailableAgent {
        label: label.to_string(),
        tool: tool.to_string(),
        invocation: invocation.to_string(),
        description: None,
    }
}

/// The single line containing `needle` (for SGR-on-the-right-line
/// assertions), or "" if none.
pub(crate) fn line_with<'a>(lines: &'a [String], needle: &str) -> &'a str {
    lines
        .iter()
        .find(|l| l.contains(needle))
        .map(String::as_str)
        .unwrap_or("")
}

pub(crate) const REVERSE: &str = "\x1b[7m"; // emit_selected band

pub(crate) fn plan_state(stem: &str, waiting_on: WaitingOn) -> PlanWorkState {
    PlanWorkState {
        plan: PlanKey::parse(stem).unwrap(),
        sha: Some(CommitSha::parse(&format!("{:0<40}", "abc123")).unwrap()),
        gate: CommitGateState::Unreviewed,
        waiting_on,
        touched_code: false,
    }
}

pub(crate) fn reviewer_missing(label: &str) -> WaitingOn {
    WaitingOn::ReviewerApprovalsMissing {
        missing: NonEmptyVec::new(vec![AgentLabel::parse(label).unwrap()]).unwrap(),
    }
}

pub(crate) fn snap(plans: Vec<PlanWorkState>, queue: Vec<&str>) -> StatusSnapshot {
    StatusSnapshot {
        forks: Vec::new(),
        repo_path: "/r".into(),
        basename: "r".into(),
        branch: Some("master".into()),
        head_sha: Some(format!("{:0<40}", "deadbeef")),
        head_subject: None,
        dirty: None,
        plans,
        last_finished: None,
        blocks: Vec::new(),
        queue: queue
            .into_iter()
            .map(|name| crate::cli::status::QueueItemView {
                priority: 500,
                name: name.to_string(),
            })
            .collect(),
        master: Some("claude".into()),
        agents: Vec::new(),
        stash: Vec::new(),
        log_rows: Vec::new(),
        github_events: Vec::new(),
        log_decorations: Default::default(),
        pr_reviews: Vec::new(),
        ad_hoc: Vec::new(),
        head_correction: None,
    }
}

/// Roster rows for activity-projection tests: candidates come from
/// the roster (tui-adhoc-review-activity), so fixtures asserting
/// awaited/spinner state must declare who is on the team.
pub(crate) fn with_agents(
    mut s: StatusSnapshot,
    agents: &[(&str, crate::cli::teams_config::RosterRole)],
) -> StatusSnapshot {
    s.agents = agents
        .iter()
        .map(|(label, role)| crate::cli::status::AgentAutoRow {
            label: label.to_string(),
            role: *role,
            auto_mode: clank_core::vocab::AutoMode::On,
            tool: "claude".to_string(),
            invocation: "claude".to_string(),
            session: None,
            attending: None,
        })
        .collect();
    s
}

/// Visible text: ANSI escapes removed, trailing pad trimmed.
/// Tests pin what the EYE sees.
pub(crate) fn visible(line: &str) -> String {
    visible_untrimmed(line).trim_end().to_string()
}

/// `visible` without the trailing-pad trim — for asserting the
/// bar's exact padded width. Strips both CSI color sequences
/// (`ESC [ … m`) and OSC 8 hyperlinks (`ESC ] … ST`); the latter
/// matters because a URL like `github.com` contains an `m`, so
/// the CSI-only scan would stop mid-URL.
pub(crate) fn visible_untrimmed(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // OSC: ESC ] … terminated by ST (ESC \) or BEL.
            Some(']') => {
                while let Some(e) = chars.next() {
                    if e == '\x07' {
                        break;
                    }
                    if e == '\x1b' {
                        chars.next(); // consume the ST's `\`
                        break;
                    }
                }
            }
            // CSI: ESC [ … terminated by a final byte (here `m`).
            _ => {
                for e in chars.by_ref() {
                    if e == 'm' {
                        break;
                    }
                }
            }
        }
    }
    out
}

pub(crate) fn pr_work(round: u64, missing: &[&str]) -> clank_core::wait::PrReviewWorkState {
    clank_core::wait::PrReviewWorkState {
        pr: 5,
        repo: "LLFourn/clank".into(),
        round,
        gate: clank_core::vocab::CommitGateState::Unreviewed,
        missing_reviewers: missing
            .iter()
            .map(|l| AgentLabel::parse(l).unwrap())
            .collect(),
    }
}

pub(crate) fn pr_awaiting(missing: &[&str]) -> clank_core::wait::PrReviewWorkState {
    clank_core::wait::PrReviewWorkState {
        pr: 1,
        repo: "o/r".into(),
        round: 1,
        gate: clank_core::vocab::CommitGateState::Unreviewed,
        missing_reviewers: missing
            .iter()
            .map(|l| AgentLabel::parse(l).unwrap())
            .collect(),
    }
}
