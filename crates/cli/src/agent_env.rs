//! Env-var reads for agent context — the single CLI-side place
//! that knows about `CLAUDE_CODE_SESSION_ID`, `CODEX_THREAD_ID`,
//! and `CLANK_AGENT`. Every command that builds `IdentityInputs`
//! goes through here.

use std::env;
use std::path::Path;

use clank_core::ids::{AgentLabel, SessionId};
use clank_core::vocab::Tool;
use clank_core::{IdentityInputs, ResolveError, resolve_agent_identity};

use crate::agent_store::load_all_agent_configs_lossy;

const ENV_CLAUDE_SESSION: &str = "CLAUDE_CODE_SESSION_ID";
const ENV_CODEX_SESSION: &str = "CODEX_THREAD_ID";
const ENV_CLANK_AGENT: &str = "CLANK_AGENT";

/// Try to detect (tool, session_id) from the env vars set by the
/// running agent. Returns `Ok(Some(...))` when exactly one of the
/// session env vars is set (the common case); `Ok(None)` when
/// neither is set; `Err(BothToolsDetected)` when both are set
/// (e.g. a codex shell spawned from claude leaking
/// `CLAUDE_CODE_SESSION_ID` into the child env). Callers that
/// require a session bind the OK-None case to their own error
/// shape.
pub fn detect_session_from_env() -> Result<Option<(Tool, SessionId)>, EnvError> {
    let claude = env::var(ENV_CLAUDE_SESSION).ok();
    let codex = env::var(ENV_CODEX_SESSION).ok();
    match (claude, codex) {
        (Some(claude_val), Some(codex_val)) => Err(EnvError::BothToolsDetected {
            claude_session: claude_val,
            codex_session: codex_val,
        }),
        (Some(raw), None) => Ok(Some((Tool::Claude, parse_session_id(&raw, Tool::Claude)?))),
        (None, Some(raw)) => Ok(Some((Tool::Codex, parse_session_id(&raw, Tool::Codex)?))),
        (None, None) => Ok(None),
    }
}

/// Read the explicit-override label from `CLANK_AGENT`. Empty
/// string is treated as "unset" so users can clear the override
/// with `CLANK_AGENT= clank ...`.
pub fn explicit_label_from_env() -> Result<Option<AgentLabel>, EnvError> {
    let Ok(raw) = env::var(ENV_CLANK_AGENT) else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    AgentLabel::parse(&raw)
        .map(Some)
        .map_err(|e| EnvError::InvalidLabel {
            var: ENV_CLANK_AGENT,
            value: raw,
            reason: e.to_string(),
        })
}

fn parse_session_id(raw: &str, tool: Tool) -> Result<SessionId, EnvError> {
    SessionId::parse(raw).map_err(|e| EnvError::InvalidSessionId {
        var: match tool {
            Tool::Claude => ENV_CLAUDE_SESSION,
            Tool::Codex => ENV_CODEX_SESSION,
        },
        value: raw.to_string(),
        reason: e.to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvError {
    /// Both `CLAUDE_CODE_SESSION_ID` and `CODEX_THREAD_ID` are
    /// set in the environment. Almost always means an env var
    /// leaked from a parent shell; the caller should disambiguate
    /// (e.g. via an explicit `--tool` flag in the hook config,
    /// which is exactly how `clank stop-hook` will resolve it).
    /// Carries the raw values for diagnostics — the user needs
    /// to know which one to expect to keep.
    BothToolsDetected {
        claude_session: String,
        codex_session: String,
    },
    InvalidSessionId {
        var: &'static str,
        value: String,
        reason: String,
    },
    InvalidLabel {
        var: &'static str,
        value: String,
        reason: String,
    },
}

impl std::fmt::Display for EnvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvError::BothToolsDetected {
                claude_session,
                codex_session,
            } => write!(
                f,
                "both CLAUDE_CODE_SESSION_ID and CODEX_THREAD_ID are set \
                 (CLAUDE_CODE_SESSION_ID={claude_session:?}, \
                 CODEX_THREAD_ID={codex_session:?}) — ambiguous. One probably \
                 leaked from a parent shell; restart the inner agent or \
                 pass --tool explicitly."
            ),
            EnvError::InvalidSessionId { var, value, reason } => {
                write!(f, "{var}={value:?} is not a valid session id: {reason}")
            }
            EnvError::InvalidLabel { var, value, reason } => {
                write!(f, "{var}={value:?} is not a valid agent label: {reason}")
            }
        }
    }
}

impl std::error::Error for EnvError {}

/// High-level wrapper: read env, load this repo's agent configs,
/// and call the pure `resolve_agent_identity`. The single way
/// any non-hook CLI command finds out "who am I" — used by
/// `clank auto`, `clank wfw`, `clank feedback write` (later),
/// and `clank doctor`.
///
/// Stop-hook callers go through a different path because they
/// receive `session_id` via stdin and `tool` via `--tool`, not
/// from env — see `cli::stop_hook` (later commit).
pub fn resolve_identity_from_env(repo: &Path) -> anyhow::Result<AgentLabel> {
    // Highest precedence: explicit CLANK_AGENT override. Short-
    // circuit BEFORE touching session env — otherwise lower-
    // precedence env state (BothToolsDetected, an unparseable
    // session id, etc.) could fail the command even though the
    // resolver would happily return the explicit label. This
    // mirrors the pure resolver's precedence rule and is what
    // codex's review on `99ce55b` flagged.
    if let Some(label) = explicit_label_from_env()? {
        return Ok(label);
    }

    let detected = detect_session_from_env()?;
    let agent_configs = load_all_agent_configs_lossy(repo)?;
    let (tool, session_id_owned) = match detected {
        Some((t, sid)) => (t, Some(sid)),
        None => {
            // No session detected. The resolver will return
            // NoSession; we map that to a friendly error below.
            // Default tool to Claude — it doesn't matter since
            // the lookup path won't run without a session id.
            (Tool::Claude, None)
        }
    };
    let inputs = IdentityInputs {
        tool,
        explicit_label: None,
        session_id: session_id_owned.as_ref(),
        agent_configs: &agent_configs,
    };
    resolve_agent_identity(&inputs).map_err(|e| match e {
        ResolveError::NoSession => anyhow::anyhow!(
            "no session detected — pass --author <label>, set CLANK_AGENT, \
             or run inside claude/codex"
        ),
        ResolveError::NoAgentForSession { session_id, tool } => anyhow::anyhow!(
            "no agent is bound to {tool} session {sid} in this repo — run \
             `clank as <label>` first (inside this session) or `clank init` \
             to bootstrap",
            tool = tool.as_str(),
            sid = session_id.as_str(),
        ),
    })
}
