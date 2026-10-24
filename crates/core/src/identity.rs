//! Pure resolver: maps the calling process to an [`AgentLabel`].
//!
//! Every CLI surface that needs to know "who am I" builds a [`Caller`]
//! from env + stdin and calls [`resolve_agent_identity`] with the
//! repo's agent configs. The function is pure: no I/O, no clock, no
//! env access.
//!
//! A caller inside a harness session is that session, and only the
//! session's binding names it. `CLANK_AGENT` is inherited by every
//! descendant, including a shared Codex app-server daemon that serves
//! sessions from other panes, so it can't say which session it was
//! set for and never outranks a binding. Outside any session it is
//! the manual override it was meant to be. Neither case falls back to
//! guessing by tool: writing feedback under the wrong label is worse
//! than refusing to act.

use crate::agent_config::AgentConfig;
use crate::ids::{AgentLabel, SessionId};
use crate::vocab::Tool;

/// Who is asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller<'a> {
    /// Running outside any harness session. `named` is the
    /// `CLANK_AGENT` override, if set.
    Outside { named: Option<AgentLabel> },
    /// Running in this harness session.
    Session { tool: Tool, id: &'a SessionId },
}

/// Why the resolver couldn't pick a label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// Outside any harness session and no `CLANK_AGENT` override.
    /// Fix: pass `--author` explicitly or run inside an agent.
    NoSession,
    /// In a harness session that no agent config in this repo has
    /// bound. Fix: `clank as <label>` inside that session.
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
                 `clank as <label>` inside this session"
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

/// The agent whose binding holds `id` for `tool`. Both must match: a
/// claude and a codex session id can collide, however unlikely.
pub fn bound_to(
    tool: Tool,
    id: &SessionId,
    agent_configs: &[(AgentLabel, AgentConfig)],
) -> Option<AgentLabel> {
    agent_configs
        .iter()
        .find(|(_, cfg)| {
            cfg.session
                .as_ref()
                .is_some_and(|s| &s.id == id && s.tool == tool)
        })
        .map(|(label, _)| label.clone())
}

/// Resolve the calling agent's label.
pub fn resolve_agent_identity(
    caller: &Caller<'_>,
    agent_configs: &[(AgentLabel, AgentConfig)],
) -> Result<AgentLabel, ResolveError> {
    match caller {
        Caller::Outside { named: Some(label) } => Ok(label.clone()),
        Caller::Outside { named: None } => Err(ResolveError::NoSession),
        Caller::Session { tool, id } => {
            bound_to(*tool, id, agent_configs).ok_or_else(|| ResolveError::NoAgentForSession {
                session_id: (*id).clone(),
                tool: *tool,
            })
        }
    }
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
                transcript: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn outside_a_session_the_name_is_the_identity() {
        let configs = [(label("alice"), cfg_with_session("abc12345", Tool::Claude))];
        let caller = Caller::Outside {
            named: Some(label("zoe")),
        };
        assert_eq!(
            resolve_agent_identity(&caller, &configs).unwrap(),
            label("zoe")
        );
        assert_eq!(
            resolve_agent_identity(&Caller::Outside { named: None }, &configs),
            Err(ResolveError::NoSession)
        );
    }

    #[test]
    fn session_lookup_matches_id_and_tool() {
        let alice = (label("alice"), cfg_with_session("abc12345", Tool::Claude));
        let bob = (label("bob"), cfg_with_session("def67890", Tool::Codex));
        let configs = [alice, bob];
        let id = sid("def67890");
        let caller = Caller::Session {
            tool: Tool::Codex,
            id: &id,
        };
        assert_eq!(
            resolve_agent_identity(&caller, &configs).unwrap(),
            label("bob")
        );
    }

    #[test]
    fn session_lookup_requires_matching_tool() {
        let configs = [(label("alice"), cfg_with_session("abc12345", Tool::Claude))];
        let id = sid("abc12345");
        let caller = Caller::Session {
            tool: Tool::Codex,
            id: &id,
        };
        assert_eq!(
            resolve_agent_identity(&caller, &configs),
            Err(ResolveError::NoAgentForSession {
                session_id: sid("abc12345"),
                tool: Tool::Codex,
            })
        );
    }

    #[test]
    fn agent_without_session_is_skipped_not_short_circuited() {
        let alice_no_session = (label("alice"), AgentConfig::default());
        let bob = (label("bob"), cfg_with_session("abc12345", Tool::Claude));
        let configs = [alice_no_session, bob];
        let id = sid("abc12345");
        let caller = Caller::Session {
            tool: Tool::Claude,
            id: &id,
        };
        assert_eq!(
            resolve_agent_identity(&caller, &configs).unwrap(),
            label("bob")
        );
    }
}
