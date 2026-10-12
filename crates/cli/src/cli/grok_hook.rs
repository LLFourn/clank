//! Grok's native SessionStart binds the session; Stop asks for an ordinary
//! background wait when none is running. The monitor owns wait admission and
//! delivery. This adapter never rewrites a tool call or waits for work itself.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use clank_core::{AutoMode, SessionId, Tool};
use serde::Deserialize;

use crate::agent_env::{Ambient, Process, resolve_identity_for_hook_in};
use crate::agent_store::{binding_matches, load_agent_config};

const ARM: &str = "Run `clank wait` as one background terminal task (block_until_ms: 0, with no positive timeout), then finish the turn. Its completion wakes you. Do not poll it or launch a second wait.";

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

fn handle(input: &Input, repo: &Path, env: &dyn Ambient) -> Result<Option<&'static str>> {
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
            crate::agent_store::resolve_role(repo, &label)?;
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
    let cfg = load_agent_config(repo, &label)?;
    if super::team::resolve_effective_auto_mode(cfg.as_ref(), env.home().as_deref())
        == AutoMode::Off
    {
        return Ok(None);
    }
    crate::agent_store::resolve_role(repo, &label)?;
    let tasks = input
        .background_tasks
        .as_ref()
        .context("native Grok Stop omitted backgroundTasks")?;
    if tasks.iter().any(|task| {
        task.kind == "shell"
            && task.status == "running"
            && task.command.as_deref().is_some_and(is_wait)
    }) {
        return Ok(None);
    }
    // Grok owns immediate task completion and queues its continuation. If the
    // requested wait already finished, allow the Stop continuation to end.
    // Never repeatedly request a broken/failed wait within the same turn.
    if input.stop_hook_active {
        return Ok(None);
    }
    Ok(Some(ARM))
}

/// The repo a hook acts for: `--repo` exactly, else the worktree holding
/// the session's cwd.
fn hook_repo(explicit: Option<&Path>, cwd: &Path) -> Result<PathBuf> {
    match explicit {
        Some(p) => super::resolve_repo(Some(p)),
        None => super::resolve_cwd_repo(cwd),
    }
}

pub(super) fn hook(args: super::StopHookArgs) -> Result<()> {
    let result = (|| -> Result<Option<&'static str>> {
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
        handle(&input, &repo, &Process)
    })();
    match result {
        Ok(Some(reason)) => println!(
            "{}",
            serde_json::json!({"decision":"block", "reason":reason})
        ),
        Ok(None) => {}
        Err(error) => eprintln!(
            "clank: Grok lifecycle unavailable: {error:#}. Pending work remains intact; check setup or use `clank auto off` for manual operation."
        ),
    }
    Ok(())
}

#[cfg(test)]
#[path = "grok_hook_tests.rs"]
mod tests;
