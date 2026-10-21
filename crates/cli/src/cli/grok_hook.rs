//! Grok's native lifecycle. SessionStart binds the session and SessionEnd
//! releases it. A Stop that ends a turn is reported to the monitor with
//! what it shows of a running `clank wait`; the monitor decides whether
//! the agent is asked for one ([`clank_core::turn_policy::handoff_verdict`])
//! and this adapter frames the answer. It never rewrites a tool call or
//! waits for work itself.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use clank_core::turn_policy::Handoff;
use clank_core::{AgentLabel, SessionId, Tool};
use serde::Deserialize;

use super::turn_wire::{Frame, Report, WireVerdict};
use crate::agent_env::{Ambient, Process, resolve_identity_for_hook_in};
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
    background_tasks: Option<Vec<Task>>,
}

#[derive(Debug, Deserialize)]
struct Task {
    #[serde(rename = "type")]
    kind: String,
    status: String,
    command: Option<String>,
}

// A native task description is evidence of a running command, never an
// authorisation token. The wait validates its session and repository itself.
fn is_wait(command: &str) -> bool {
    let Ok(words) = shell_words::split(command) else {
        return false;
    };
    let words = words.strip_prefix(&["exec".to_owned()]).unwrap_or(&words);
    matches!(words, [program, subcommand, ..]
        if Path::new(program).file_name() == Some(std::ffi::OsStr::new("clank"))
            && subcommand == "wait")
}

/// Route one native event: bind or release on the session's ends, and for
/// a bound Stop that ended a turn, the evidence the monitor decides on.
fn route(input: &Input, repo: &Path, env: &dyn Ambient) -> Result<Option<(AgentLabel, Handoff)>> {
    if input.subagent_type.is_some() {
        return Ok(None);
    }
    ensure!(
        super::resolve_cwd_repo(&input.cwd)? == repo,
        "native Grok cwd does not match the Clank repository"
    );
    let label = resolve_identity_for_hook_in(repo, Tool::Grok, &input.session_id, env)?;
    if input.hook_event_name == "session_start" {
        if env.var(crate::agent_env::ENV_CLANK_AGENT).is_some() {
            crate::agent_store::resolve_tool(repo, &label)?;
            crate::agent_store::bind_session_to_agent(repo, &label, Tool::Grok, &input.session_id)?;
        }
        return Ok(None);
    }
    if input.hook_event_name == "session_end" {
        if binding_matches(repo, &label, Tool::Grok, input.session_id.as_str()) {
            crate::agent_store::release_binding(repo, &label, &input.session_id)?;
        }
        return Ok(None);
    }
    if input.hook_event_name != "stop"
        || input.reason.as_deref() != Some("end_turn")
        || !binding_matches(repo, &label, Tool::Grok, input.session_id.as_str())
    {
        return Ok(None);
    }
    let tasks = input
        .background_tasks
        .as_ref()
        .context("native Grok Stop omitted backgroundTasks")?;
    let wait_running = tasks.iter().any(|task| {
        task.kind == "shell"
            && task.status == "running"
            && task.command.as_deref().is_some_and(is_wait)
    });
    Ok(Some((
        label,
        Handoff {
            session: input.session_id.clone(),
            wait_running,
            continued: input.stop_hook_active,
        },
    )))
}

/// The repo a hook acts for: `--repo` exactly, else the worktree holding
/// the session's cwd.
fn hook_repo(explicit: Option<&Path>, cwd: &Path) -> Result<PathBuf> {
    match explicit {
        Some(p) => super::resolve_repo(Some(p)),
        None => super::resolve_cwd_repo(cwd),
    }
}

/// How long a Stop waits for the monitor's answer.
const HANDOFF_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) async fn hook(args: super::StopHookArgs) -> Result<()> {
    let routed = (|| -> Result<Option<(PathBuf, AgentLabel, Handoff)>> {
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
        Ok(route(&input, &repo, &Process)?.map(|(label, handoff)| (repo, label, handoff)))
    })();
    let frame = match routed {
        Ok(None) => return Ok(()),
        Ok(Some((repo, label, handoff))) => {
            let session = handoff.session.as_str().to_string();
            let asked = super::stop_hook::answer_once(
                &repo,
                &label,
                &session,
                Report::Handoff {
                    handoff,
                    deadline: super::turn_custody::now_ms() + HANDOFF_BUDGET.as_millis() as u64,
                },
                HANDOFF_BUDGET,
            );
            still_bound(&repo, &label, &session, asked).await
        }
        Err(e) => Frame::bare(WireVerdict::Diagnostic {
            message: format!("{e:#}"),
        }),
    };
    let (out, err) = wire(&frame);
    if let Some(out) = out {
        println!("{out}");
    }
    if let Some(err) = err {
        eprintln!("{err}");
    }
    Ok(())
}

/// The monitor's answer once `asked` resolves, while `session` still holds
/// the label: one authorized for a binding replaced while it was in
/// transit, or while its child was collected, is not this session's.
async fn still_bound(
    repo: &Path,
    label: &AgentLabel,
    session: &str,
    asked: impl std::future::Future<Output = Frame>,
) -> Frame {
    let frame = asked.await;
    match binding_matches(repo, label, Tool::Grok, session) {
        true => frame,
        false => Frame::bare(WireVerdict::Silent {
            why: clank_core::SilentReason::StaleBinding,
        }),
    }
}

/// Grok's wire for a verdict: a request for the wait blocks the Stop with
/// its reason; a diagnostic goes to stderr; anything else says nothing.
fn wire(frame: &Frame) -> (Option<String>, Option<String>) {
    match &frame.verdict {
        WireVerdict::Notify => (
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
