//! Pure resolver: maps a session (or explicit override) to an
//! [`AgentLabel`].
//!
//! Every CLI surface that needs to know "who am I" (the stop-hook
//! adapter, `clank auto`, `clank wait`, `clank doctor`) builds
//! [`IdentityInputs`] from env + stdin + on-disk config and calls
//! [`resolve_agent_identity`]. The function is pure: no I/O, no
//! clock, no env access — the CLI does all of that, then hands
//! the resolver a snapshot.
//!
//! Precedence (high → low):
//!
//! 1. [`IdentityInputs::explicit_label`] from the `CLANK_AGENT`
//!    env var. Lets advanced users force an identity regardless
//!    of session bindings.
//! 2. Session lookup: scan `agent_configs` for one whose
//!    [`AgentConfig::session`] matches both [`SessionId`] AND
//!    [`Tool`]. (Both must match — a claude session id and a
//!    codex session id can collide, however unlikely.)
//! 3. Error. No tool-name fallback (a previous design); writing
//!    feedback under the wrong label is worse than refusing to
//!    act.
//!
//! On `Err`, the caller's job is to emit an actionable message
//! ("no agent set up for this session — run `clank init` or
//! `clank as <label>`") and exit cleanly.

use crate::agent_config::AgentConfig;
use crate::ids::{AgentLabel, SessionId};
use crate::vocab::Tool;

/// Snapshot of everything the resolver needs to decide. Built by
/// the CLI from env + stdin + disk; consumed by the pure
/// resolver. No fields are `Option` for ergonomics — `session_id`
/// being `None` is a real distinct state (e.g. running `clank
/// status` outside any agent) that the resolver maps to a
/// distinct error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityInputs<'a> {
    /// Which agent CLI is wrapping this invocation. Passed
    /// explicitly via `--tool` from the hook config; not detected
    /// from env (env vars leak across nested shells; see plan D4
    /// "tool detection caveat").
    pub tool: Tool,
    /// `CLANK_AGENT` env override. Wins if set.
    pub explicit_label: Option<AgentLabel>,
    /// Current session id read from
    /// `CLAUDE_CODE_SESSION_ID` / `CODEX_THREAD_ID` (or supplied
    /// by hook stdin). `None` when running outside any agent.
    pub session_id: Option<&'a SessionId>,
    /// Every agent config in this repo, paired with its label.
    /// Resolver scans these for a session match.
    pub agent_configs: &'a [(AgentLabel, AgentConfig)],
}

/// Why the resolver couldn't pick a label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// Caller wasn't running inside an agent (no session id env
    /// var, no hook stdin) AND no `CLANK_AGENT` override. Fix:
    /// pass `--author` explicitly or run inside claude/codex/grok/opencode.
    NoSession,
    /// Running inside an agent, session id resolved, but no agent
    /// config in this repo has bound that session id. Fix: run
    /// `clank init` (inside this session) or `clank as <label>`.
    NoAgentForSession {
        /// Echoed back so the CLI can include it in the
        /// diagnostic for debugging.
        session_id: SessionId,
        tool: Tool,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NoSession => f.write_str(
                "no session detected — pass --author <label> or run \
                 inside claude/codex/grok/opencode",
            ),
            ResolveError::NoAgentForSession { session_id, tool } => write!(
                f,
                "no agent set up for {tool} session {session_id} — run \
                 `clank init` (inside this session) or `clank as <label>`"
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Resolve the calling agent's label. See module docs for the
/// precedence rule.
pub fn resolve_agent_identity(inputs: &IdentityInputs<'_>) -> Result<AgentLabel, ResolveError> {
    if let Some(explicit) = &inputs.explicit_label {
        return Ok(explicit.clone());
    }
    let Some(session_id) = inputs.session_id else {
        return Err(ResolveError::NoSession);
    };
    for (label, cfg) in inputs.agent_configs {
        if let Some(s) = &cfg.session
            && &s.id == session_id
            && s.tool == inputs.tool
        {
            return Ok(label.clone());
        }
    }
    Err(ResolveError::NoAgentForSession {
        session_id: session_id.clone(),
        tool: inputs.tool,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_config::Session;
    use crate::vocab::AutoMode;

    fn label(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
    }
    fn sid(s: &str) -> SessionId {
        SessionId::parse(s).unwrap()
    }
    fn cfg_with_session(id: &str, tool: Tool) -> AgentConfig {
        AgentConfig {
            auto_mode: Some(AutoMode::On),
            session: Some(Session {
                id: sid(id),
                tool,
                updated_at: "2026-05-23T00:00:00+00:00".into(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn explicit_label_wins_even_with_matching_session() {
        let alice = (label("alice"), cfg_with_session("abc12345", Tool::Claude));
        let configs = [alice];
        let id = sid("abc12345");
        let inputs = IdentityInputs {
            tool: Tool::Claude,
            explicit_label: Some(label("zoe")),
            session_id: Some(&id),
            agent_configs: &configs,
        };
        assert_eq!(resolve_agent_identity(&inputs).unwrap(), label("zoe"));
    }

    #[test]
    fn session_lookup_matches_id_and_tool() {
        let alice = (label("alice"), cfg_with_session("abc12345", Tool::Claude));
        let bob = (label("bob"), cfg_with_session("def67890", Tool::Codex));
        let configs = [alice, bob];
        let id = sid("def67890");
        let inputs = IdentityInputs {
            tool: Tool::Codex,
            explicit_label: None,
            session_id: Some(&id),
            agent_configs: &configs,
        };
        assert_eq!(resolve_agent_identity(&inputs).unwrap(), label("bob"));
    }

    #[test]
    fn session_lookup_requires_matching_tool() {
        // Same id bound to alice-as-claude; lookup is for codex.
        // Must miss; resolver does not silently accept the wrong
        // tool.
        let alice = (label("alice"), cfg_with_session("abc12345", Tool::Claude));
        let configs = [alice];
        let id = sid("abc12345");
        let inputs = IdentityInputs {
            tool: Tool::Codex,
            explicit_label: None,
            session_id: Some(&id),
            agent_configs: &configs,
        };
        let err = resolve_agent_identity(&inputs).unwrap_err();
        assert!(matches!(err, ResolveError::NoAgentForSession { .. }));
    }

    #[test]
    fn no_session_id_returns_no_session_err() {
        let configs: [(AgentLabel, AgentConfig); 0] = [];
        let inputs = IdentityInputs {
            tool: Tool::Claude,
            explicit_label: None,
            session_id: None,
            agent_configs: &configs,
        };
        assert_eq!(
            resolve_agent_identity(&inputs),
            Err(ResolveError::NoSession)
        );
    }

    #[test]
    fn no_matching_agent_returns_no_agent_for_session() {
        let configs: [(AgentLabel, AgentConfig); 0] = [];
        let id = sid("abc12345");
        let inputs = IdentityInputs {
            tool: Tool::Claude,
            explicit_label: None,
            session_id: Some(&id),
            agent_configs: &configs,
        };
        let err = resolve_agent_identity(&inputs).unwrap_err();
        assert_eq!(
            err,
            ResolveError::NoAgentForSession {
                session_id: sid("abc12345"),
                tool: Tool::Claude,
            }
        );
    }

    #[test]
    fn agent_without_session_is_skipped_not_short_circuited() {
        // alice has no session binding; bob does. The resolver
        // must skip alice (no session) and continue to bob — not
        // return Err on alice's None and miss bob entirely.
        let alice_no_session = (label("alice"), AgentConfig::default());
        let bob = (label("bob"), cfg_with_session("abc12345", Tool::Claude));
        let configs = [alice_no_session, bob];
        let id = sid("abc12345");
        let inputs = IdentityInputs {
            tool: Tool::Claude,
            explicit_label: None,
            session_id: Some(&id),
            agent_configs: &configs,
        };
        assert_eq!(resolve_agent_identity(&inputs).unwrap(), label("bob"));
    }
}
