//! Grok's native lifecycle. SessionStart binds the session and SessionEnd
//! releases it. A Stop that ends a turn is reported to the monitor, which
//! knows from its own registrations whether the session's `clank wait` is
//! running and decides whether the agent is asked for one
//! ([`clank_core::turn_policy::handoff_verdict`]); this adapter frames the
//! answer. The Stop's `backgroundTasks` go with it, read only to resolve
//! and end the agent's own declarations. It never rewrites a tool call and
//! never waits for work itself.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use clank_core::harness::grok::{self, Event as Input, Kind};
use clank_core::turn_policy::TaskSnapshot;
use clank_core::{AgentLabel, Tool};

use super::turn_wire::{Frame, Report, WireVerdict};
use crate::agent_env::{Ambient, Process};
use crate::agent_store::binding_matches;

/// Route one native event: bind or release on the session's ends, and for
/// a bound Stop that ended a turn, the label it reports for.
fn route(input: &Input, repo: &Path, env: &dyn Ambient) -> Result<Option<AgentLabel>> {
    let kind = input.kind();
    if kind == Kind::Subagent {
        return Ok(None);
    }
    ensure!(
        super::resolve_cwd_repo(&input.cwd)? == repo,
        "native Grok cwd does not match the Clank repository"
    );
    if kind == Kind::SessionStart {
        let launched_as = crate::agent_env::explicit_label_in(env)?;
        if let crate::agent_env::Start::Bootstrap(label) = crate::agent_env::session_start(
            repo,
            Tool::Grok,
            &input.session_id,
            launched_as.as_ref(),
        )? {
            crate::agent_store::bind_session_to_agent(repo, &label, Tool::Grok, &input.session_id)?;
        }
        return Ok(None);
    }
    let Some(label) = crate::agent_env::bound_agent(repo, Tool::Grok, &input.session_id)? else {
        return Ok(None);
    };
    match kind {
        Kind::SessionEnd => {
            crate::agent_store::release_binding(repo, &label, &input.session_id)?;
            Ok(None)
        }
        Kind::TurnEnd => Ok(Some(label)),
        Kind::Subagent | Kind::SessionStart | Kind::Other => Ok(None),
    }
}

/// The repo a hook acts for: `--repo` exactly, else the worktree holding
/// the session's cwd.
fn hook_repo(explicit: Option<&Path>, cwd: &Path) -> Result<PathBuf> {
    match explicit {
        Some(p) => super::resolve_repo(Some(p)),
        None => super::resolve_cwd_repo(cwd),
    }
}

/// What a routed Stop reports: an omitted `backgroundTasks` is no
/// listing, never an empty one.
fn report(input: Input, hook_started: clank_core::turn_policy::Ms, continued: bool) -> Report {
    Report::Handoff {
        session: input.session_id,
        continued,
        tasks: TaskSnapshot::listed(input.background_tasks),
        hook_started: Some(hook_started),
        deadline: hook_started.saturating_add(HANDOFF_BUDGET.as_millis() as u64),
    }
}

/// Routing and the monitor exchange share this budget from hook entry.
pub(super) const HANDOFF_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// The runner starts its clock before this binary enters the hook. Leave
/// ten seconds for process startup and framing, in addition to the entire
/// exchange and its bounded cooperative child shutdown. Installation and
/// validation use this same ceiling; a five-second runner cannot carry it.
pub(super) const RUNNER_TIMEOUT_SECS: u64 =
    HANDOFF_BUDGET.as_secs() + super::stop_hook::ABANDON_GRACE.as_secs() + 10;

fn remaining_budget(elapsed: std::time::Duration) -> std::time::Duration {
    HANDOFF_BUDGET.saturating_sub(elapsed)
}

pub(super) async fn hook(args: super::StopHookArgs) -> Result<()> {
    let started = std::time::Instant::now();
    let hook_started = super::turn_custody::now_ms();
    let routed = (|| -> Result<Option<(PathBuf, AgentLabel, Report)>> {
        let mut bytes = Vec::new();
        std::io::stdin()
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "native Grok hook input exceeds 1 MiB"
        );
        let input = grok::parse(&bytes).context("unsupported native Grok hook payload")?;
        let repo = hook_repo(args.repo.as_deref(), &input.cwd)?;
        if !repo.join(".clank").is_dir() {
            return Ok(None);
        }
        let Some(label) = route(&input, &repo, &Process)? else {
            return Ok(None);
        };
        let continued = input.stop_hook_active;
        Ok(Some((repo, label, report(input, hook_started, continued))))
    })();
    match routed {
        Ok(None) => {}
        Ok(Some((repo, label, report))) => {
            hand_off(
                &super::stop_hook::Spawner,
                &repo,
                &label,
                report,
                remaining_budget(started.elapsed()),
                &mut std::io::stdout(),
                &mut std::io::stderr(),
            )
            .await;
        }
        Err(e) => {
            print(
                &Frame::bare(WireVerdict::Diagnostic {
                    message: format!("{e:#}"),
                }),
                &mut std::io::stdout(),
                &mut std::io::stderr(),
            );
        }
    }
    Ok(())
}

/// Carry a routed Stop's `report` for `label` through a child `spawner`
/// starts, and write the answer on Grok's wire within `budget`, while the
/// report's session still holds the label.
pub(super) async fn hand_off(
    spawner: &dyn super::stop_hook::Spawn,
    repo: &Path,
    label: &AgentLabel,
    report: Report,
    budget: std::time::Duration,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) {
    let bound = report.session().as_str().to_string();
    super::stop_hook::answer_with(spawner, repo, label, &bound, report, budget, |frame| {
        print(&still_bound(repo, label, &bound, frame.clone()), out, err)
    })
    .await;
}

/// Write `frame` on Grok's wire: how writing its decision went, or `None`
/// when it has none.
fn print(
    frame: &Frame,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> Option<std::io::Result<()>> {
    let (decision, diagnostic) = wire(frame);
    if let Some(diagnostic) = diagnostic {
        let _ = writeln!(err, "{diagnostic}");
    }
    decision.map(|decision| {
        writeln!(out, "{decision}")?;
        out.flush()
    })
}

/// The monitor's answer as it is about to be written, while `session`
/// still holds the label: one authorized for a binding replaced while it
/// was in transit is not this session's.
fn still_bound(repo: &Path, label: &AgentLabel, session: &str, frame: Frame) -> Frame {
    match binding_matches(repo, label, Tool::Grok, session) {
        true => frame,
        false => Frame::bare(WireVerdict::Silent {
            why: clank_core::SilentReason::StaleBinding,
        }),
    }
}

/// Grok's wire for a verdict: a request for the wait, or a check-in, blocks
/// the Stop with its reason; so does a handoff left unanswered, with the
/// request for the wait, whose own attach asks the monitor what the Stop
/// could not; a diagnostic goes to stderr; anything else says nothing.
pub(super) fn wire(frame: &Frame) -> (Option<String>, Option<String>) {
    match &frame.verdict {
        WireVerdict::Notify | WireVerdict::Deliver { .. } => (
            Some(grok::block(frame.rendered.as_deref().unwrap_or_default())),
            None,
        ),
        WireVerdict::Unanswered { .. } => (Some(grok::block(super::wake_render::ARM_WAIT)), None),
        WireVerdict::Silent { .. } => (None, None),
        WireVerdict::Diagnostic { message } => (
            None,
            Some(format!(
                "clank: Grok lifecycle unavailable: {message}. Pending work remains intact; check \
                 setup or use `clank auto off` for manual operation."
            )),
        ),
        other => (
            None,
            Some(format!(
                "clank: Grok lifecycle got a verdict it cannot frame: {other:?}"
            )),
        ),
    }
}

#[cfg(test)]
#[path = "grok_hook_tests.rs"]
mod tests;
