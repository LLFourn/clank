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
use clank_core::turn_policy::TaskSnapshot;
use clank_core::{AgentLabel, BackgroundTask, SessionId, Tool};
use serde::Deserialize;

use super::turn_wire::{Frame, Report, WireVerdict};
use crate::agent_env::{Ambient, Process};
use crate::agent_store::binding_matches;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    hook_event_name: String,
    session_id: SessionId,
    cwd: PathBuf,
    reason: Option<String>,
    subagent_type: Option<String>,
    #[serde(default)]
    stop_hook_active: bool,
    #[serde(default)]
    background_tasks: Option<Vec<BackgroundTask>>,
}

/// Route one native event: bind or release on the session's ends, and for
/// a bound Stop that ended a turn, the label it reports for.
fn route(input: &Input, repo: &Path, env: &dyn Ambient) -> Result<Option<AgentLabel>> {
    if input.subagent_type.is_some() {
        return Ok(None);
    }
    ensure!(
        super::resolve_cwd_repo(&input.cwd)? == repo,
        "native Grok cwd does not match the Clank repository"
    );
    if input.hook_event_name == "session_start" {
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
    if input.hook_event_name == "session_end" {
        crate::agent_store::release_binding(repo, &label, &input.session_id)?;
        return Ok(None);
    }
    if input.hook_event_name != "stop" || input.reason.as_deref() != Some("end_turn") {
        return Ok(None);
    }
    Ok(Some(label))
}

/// The repo a hook acts for: `--repo` exactly, else the worktree holding
/// the session's cwd.
fn hook_repo(explicit: Option<&Path>, cwd: &Path) -> Result<PathBuf> {
    match explicit {
        Some(p) => super::resolve_repo(Some(p)),
        None => super::resolve_cwd_repo(cwd),
    }
}

/// What a routed Stop reports at `now`: an omitted `backgroundTasks` is no
/// listing, never an empty one.
fn report(
    input: Input,
    hook_started: clank_core::turn_policy::Ms,
    now: clank_core::turn_policy::Ms,
) -> Report {
    Report::Handoff {
        session: input.session_id,
        continued: input.stop_hook_active,
        tasks: TaskSnapshot::listed(input.background_tasks),
        hook_started: Some(hook_started),
        deadline: now + HANDOFF_BUDGET.as_millis() as u64,
    }
}

/// How long a Stop waits for the monitor's answer.
const HANDOFF_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) async fn hook(args: super::StopHookArgs) -> Result<()> {
    let hook_started = super::turn_custody::now_ms();
    let routed = (|| -> Result<Option<(PathBuf, AgentLabel, Input)>> {
        let mut bytes = Vec::new();
        std::io::stdin()
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "native Grok hook input exceeds 1 MiB"
        );
        let input: Input =
            serde_json::from_slice(&bytes).context("unsupported native Grok hook payload")?;
        let repo = hook_repo(args.repo.as_deref(), &input.cwd)?;
        if !repo.join(".clank").is_dir() {
            return Ok(None);
        }
        Ok(route(&input, &repo, &Process)?.map(|label| (repo, label, input)))
    })();
    match routed {
        Ok(None) => {}
        Ok(Some((repo, label, input))) => {
            let bound = input.session_id.as_str().to_string();
            super::stop_hook::answer_once(
                &repo,
                &label,
                &bound,
                report(input, hook_started, super::turn_custody::now_ms()),
                HANDOFF_BUDGET,
                |frame| print(&still_bound(&repo, &label, &bound, frame.clone())),
            )
            .await;
        }
        Err(e) => {
            print(&Frame::bare(WireVerdict::Diagnostic {
                message: format!("{e:#}"),
            }));
        }
    }
    Ok(())
}

/// Write `frame` on Grok's wire: how writing its decision went, or `None`
/// when it has none.
fn print(frame: &Frame) -> Option<std::io::Result<()>> {
    use std::io::Write;
    let (out, err) = wire(frame);
    if let Some(err) = err {
        eprintln!("{err}");
    }
    out.map(|out| {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{out}")?;
        stdout.flush()
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
/// the Stop with its reason; a diagnostic goes to stderr; anything else
/// says nothing.
fn wire(frame: &Frame) -> (Option<String>, Option<String>) {
    match &frame.verdict {
        WireVerdict::Notify | WireVerdict::Deliver { .. } => (
            Some(
                serde_json::json!({
                    "decision": "block",
                    "reason": frame.rendered.as_deref().unwrap_or_default(),
                })
                .to_string(),
            ),
            None,
        ),
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
