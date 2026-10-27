//! OpenCode 2 fork preparation. OpenCode 2 dropped the interactive
//! `--fork`, and `mini --fork` leaves the child stored in the parent's
//! directory, where OpenCode would run its tools. So the child is made with
//! `session.fork`, moved with `session.move`, confirmed with `session.get`,
//! and only then bound and launched as an ordinary `--session` resume.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::{Value, json};

use super::fork::{OpenCodeForkIntent, load_fork_spec, save_fork_spec};
use clank_core::agent_config::{LaunchConfig, Session};
use clank_core::ids::{AgentLabel, SessionId};
use clank_core::vocab::Tool;

#[cfg(test)]
#[path = "opencode_fork_tests.rs"]
pub(crate) mod tests;

/// The OpenCode the launched pane will use: preparing through any other
/// store would leave `--session` to create an empty session under the id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Backend {
    pub program: String,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
}

/// `opencode api` cannot be pointed at the server a profile's
/// `--server`/`--standalone` selects, so such a profile cannot be prepared.
pub(crate) fn validate_profile(launch: Option<&LaunchConfig>) -> anyhow::Result<()> {
    for arg in launch.into_iter().flat_map(|l| &l.args) {
        let flag = arg.split('=').next().unwrap_or(arg);
        anyhow::ensure!(
            !["--server", "--standalone"].contains(&flag),
            "an OpenCode fork cannot be prepared under a launch profile with `{flag}`: \
             `opencode api` would fork in a different session store than the one the \
             pane opens. Change the profile with `clank agent launch`"
        );
    }
    Ok(())
}

pub(crate) enum CallError {
    /// OpenCode never acted: it did not start, or refused before acting.
    Refused(anyhow::Error),
    /// The request may have acted.
    Unknown(anyhow::Error),
}

impl CallError {
    fn into_inner(self) -> anyhow::Error {
        match self {
            CallError::Refused(e) | CallError::Unknown(e) => e,
        }
    }
}

pub(crate) trait Api {
    /// `opencode api <method> --param sessionID=<session> [-d <body>]`,
    /// answering the `data` it prints.
    fn call(
        &self,
        backend: &Backend,
        method: &str,
        session: &str,
        body: Option<Value>,
    ) -> Result<Value, CallError>;
}

pub(crate) struct Cli;

impl Api for Cli {
    fn call(
        &self,
        backend: &Backend,
        method: &str,
        session: &str,
        body: Option<Value>,
    ) -> Result<Value, CallError> {
        let mut cmd = std::process::Command::new(&backend.program);
        cmd.args(["api", method, "--param", &format!("sessionID={session}")])
            .current_dir(&backend.cwd)
            .stdin(std::process::Stdio::null());
        if let Some(body) = body {
            cmd.args(["-d", &body.to_string()]);
        }
        for var in crate::agent_env::SESSION_IDENTITY_VARS {
            cmd.env_remove(var);
        }
        cmd.envs(&backend.env);
        let out = cmd.output().map_err(|e| {
            CallError::Refused(
                anyhow::Error::from(e)
                    .context(format!("running `{} api {method}`", backend.program)),
            )
        })?;
        let printed: Option<Value> = serde_json::from_slice(&out.stdout).ok();
        let failed = || {
            anyhow::anyhow!(
                "`{} api {method}` for {session} failed ({}): {}{}",
                backend.program,
                out.status,
                String::from_utf8_lossy(&out.stdout).trim(),
                String::from_utf8_lossy(&out.stderr).trim()
            )
        };
        if out.status.success() {
            if let Some(data) = printed.as_ref().and_then(|v| v.get("data")) {
                return Ok(data.clone());
            }
            // `session.move` answers 204 No Content; `session.get` is what
            // proves placement.
            if method == "session.move" && out.stdout.trim_ascii().is_empty() {
                return Ok(Value::Null);
            }
        }
        if printed.as_ref().is_some_and(names_missing_session) {
            return Err(CallError::Refused(failed()));
        }
        Err(CallError::Unknown(failed()))
    }
}

/// OpenCode's `SessionNotFoundError` is raised on lookup, before any write.
fn names_missing_session(printed: &Value) -> bool {
    [printed, printed.get("error").unwrap_or(&Value::Null)]
        .into_iter()
        .any(|v| {
            ["_tag", "name"]
                .iter()
                .any(|k| v.get(k).and_then(Value::as_str) == Some("SessionNotFoundError"))
        })
}

fn lost_reply(label: &AgentLabel, source: &str) -> String {
    format!(
        "`{label}` was not launched: its OpenCode fork of {source} was requested, but no \
         reply naming the child was recorded, so a child session may exist. Look for it with \
         `opencode session list` in the source repo, then in this fork run either \
         `clank fork recover {label} --child <id>` (the next start moves it here and \
         confirms it before binding it) or, after deleting any stray child, \
         `clank fork recover {label} --forget` (the next start forks afresh)"
    )
}

/// Fork the source (once per intent), place the child in `backend.cwd`,
/// and bind `label` to it. The intent is kept: the launch consumes it. All under the label's binding lock, so
/// concurrent starts serialize and later ones find the binding.
pub(crate) fn prepare(
    repo: &Path,
    label: &AgentLabel,
    backend: &Backend,
    superseded: Option<&Session>,
    api: &dyn Api,
) -> anyhow::Result<Session> {
    let guard = crate::agent_store::hold_binding(repo, label)?;
    let bound = crate::agent_store::load_agent_config(repo, label)?.and_then(|c| c.session);
    match bound {
        Some(s) if s.tool == Tool::OpenCode => return Ok(s),
        Some(s) if Some(&s) != superseded => anyhow::bail!(
            "`{label}` was bound to {} session {} while it was starting",
            s.tool.as_str(),
            s.id.as_str()
        ),
        _ => {}
    }
    let mut spec = load_fork_spec(repo, label)?
        .filter(|s| s.tool == Tool::OpenCode)
        .context("the OpenCode fork intent disappeared before preparation")?;
    let source = spec
        .from_session
        .clone()
        .context("an OpenCode fork intent without a source is launched fresh")?;
    let child = match spec.opencode.clone() {
        Some(OpenCodeForkIntent { child_id: Some(c) }) => c,
        Some(OpenCodeForkIntent { child_id: None }) => anyhow::bail!(lost_reply(label, &source)),
        None => {
            spec.opencode = Some(OpenCodeForkIntent { child_id: None });
            save_fork_spec(repo, label, &spec)?;
            let data = match api.call(backend, "session.fork", &source, Some(json!({}))) {
                Ok(data) => data,
                Err(CallError::Refused(e)) => {
                    spec.opencode = None;
                    save_fork_spec(repo, label, &spec)?;
                    return Err(e.context(format!(
                        "`{label}` was not launched: OpenCode did not fork {source}"
                    )));
                }
                Err(CallError::Unknown(e)) => return Err(e.context(lost_reply(label, &source))),
            };
            let child = data
                .get("id")
                .and_then(Value::as_str)
                .filter(|c| *c != source.as_str())
                .with_context(|| {
                    format!(
                        "OpenCode's fork reply names no child other than the source: {data}. {}",
                        lost_reply(label, &source)
                    )
                })?
                .to_string();
            spec.opencode = Some(OpenCodeForkIntent {
                child_id: Some(child.clone()),
            });
            save_fork_spec(repo, label, &spec)?;
            child
        }
    };
    anyhow::ensure!(
        child != source,
        "`{label}`'s recorded fork child is its source {source}; it is never moved"
    );
    let child_id = SessionId::parse(&child)
        .map_err(|e| anyhow::anyhow!("OpenCode fork child `{child}` is not a session id: {e}"))?;
    let unplaced = |e: anyhow::Error| {
        e.context(format!(
            "`{label}` was not launched: fork child {child} is not confirmed in `{}`; the \
             next start retries it without forking again",
            backend.cwd.display()
        ))
    };
    api.call(
        backend,
        "session.move",
        &child,
        Some(json!({ "directory": backend.cwd })),
    )
    .map_err(|e| unplaced(e.into_inner()))?;
    let info = api
        .call(backend, "session.get", &child, None)
        .map_err(|e| unplaced(e.into_inner()))?;
    let stored = info
        .pointer("/location/directory")
        .or_else(|| info.get("directory"))
        .and_then(Value::as_str);
    if !stored.is_some_and(|d| same_dir(Path::new(d), &backend.cwd)) {
        return Err(unplaced(anyhow::anyhow!(
            "OpenCode stores it in {}",
            stored.unwrap_or("no directory")
        )));
    }
    let session = crate::agent_store::reserve_opencode_locked(&guard, superseded, || child_id)
        .with_context(|| format!("`{label}` was not launched: its session could not be bound"))?;
    Ok(session)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

pub(crate) enum Recovery {
    Adopt(String),
    Forget,
}

/// Settle `label`'s unanswered fork request, and nothing else's.
pub(crate) fn recover(
    repo: &Path,
    label: &AgentLabel,
    recovery: Recovery,
) -> anyhow::Result<String> {
    let _guard = crate::agent_store::hold_binding(repo, label)?;
    if let Some(s) = crate::agent_store::load_agent_config(repo, label)?.and_then(|c| c.session) {
        anyhow::bail!(
            "`{label}` is bound to {} session {}; there is no fork to recover",
            s.tool.as_str(),
            s.id.as_str()
        );
    }
    let mut spec = load_fork_spec(repo, label)?
        .filter(|s| s.tool == Tool::OpenCode)
        .with_context(|| format!("`{label}` has no OpenCode fork intent in this repo"))?;
    match &spec.opencode {
        None => anyhow::bail!("`{label}`'s OpenCode fork was never requested; just start it"),
        Some(OpenCodeForkIntent { child_id: Some(c) }) => anyhow::bail!(
            "`{label}`'s OpenCode fork already has child {c}; `clank agent start {label}` \
             places and binds it"
        ),
        Some(OpenCodeForkIntent { child_id: None }) => {}
    }
    let done = match recovery {
        Recovery::Adopt(child) => {
            SessionId::parse(&child)
                .map_err(|e| anyhow::anyhow!("`{child}` is not a session id: {e}"))?;
            anyhow::ensure!(
                spec.from_session.as_deref() != Some(child.as_str()),
                "{child} is the fork's source session, not its child"
            );
            spec.opencode = Some(OpenCodeForkIntent {
                child_id: Some(child.clone()),
            });
            format!(
                "`{label}` adopts {child}; `clank agent start {label}` moves it here and \
                 confirms it before binding it"
            )
        }
        Recovery::Forget => {
            spec.opencode = None;
            format!("`{label}`'s fork request is forgotten; its next start forks afresh")
        }
    };
    save_fork_spec(repo, label, &spec)?;
    Ok(done)
}
