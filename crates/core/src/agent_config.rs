//! Typed config schema for per-agent Clank state.
//!
//! Pure data + serde round-trip. No clock, no env, no filesystem
//! — the CLI is the only place that touches the world. This type
//! describes what's written to
//! `.clank/agents/<label>/config.json` (per-agent, per-machine;
//! gitignored).
//!
//! There is intentionally no repo-shared config type. Master is
//! a per-user preference: whoever wrote the plan wants their
//! `wfw` to default to master's view; everyone else wants
//! reviewers. There's no useful repo-wide assertion of "alice is
//! THE master" — gate state is computed from the cumulative
//! participant set, not from role claims.

use serde::{Deserialize, Serialize};

use crate::ids::{AgentLabel, SessionId};
use crate::vocab::{AutoMode, Role, Tool};

/// Per-agent state for one (label, repo). Lives at
/// `.clank/agents/<label>/config.json`. Gitignored.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub auto_mode: AutoMode,
    /// Default role for this agent in this repo. Used by
    /// `wfw` / `stop-hook` when no explicit `--role` is passed.
    /// Per-user preference; two agents on the same repo can
    /// independently choose `master` without conflict (gate
    /// state ignores role claims).
    #[serde(default)]
    pub role: Role,
    /// Wait-for-work timeout as a duration string (`"30m"`,
    /// `"5m"`, `"45s"`). `None` means indefinite. Stringly typed
    /// so users editing JSON see the same form
    /// `clank wfw --timeout` accepts; validated by the CLI's
    /// existing `parse_timeout` at use site, not load site (one
    /// source of truth for the format).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wfw_timeout: Option<String>,
    /// Session this agent label is currently bound to. Written by
    /// `clank as <label>` (or `clank init` phase 2) using the
    /// `CLAUDE_CODE_SESSION_ID` / `CODEX_THREAD_ID` env var.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Session>,
}

/// One session's binding to an agent label. Stored under
/// [`AgentConfig::session`]; the resolver matches by `(id, tool)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub tool: Tool,
    /// RFC3339 / ISO-8601 timestamp of the last bind, in UTC
    /// (e.g. `"2026-05-23T06:24:47Z"`). Stringly typed so core
    /// stays free of `chrono`/`time` deps; the CLI formats with
    /// whatever clock source it has. UTC over local-offset
    /// because the `time` crate's `local-offset` codepath has
    /// known soundness issues in multi-threaded programs.
    pub updated_at: String,
}

/// The agent's effective role: their stored
/// [`AgentConfig::role`]. Pure helper kept as a separate
/// function for readability + so future changes (e.g. a
/// per-plan role override) have one place to live.
pub fn role_for(_label: &AgentLabel, config: Option<&AgentConfig>) -> Role {
    config.map(|c| c.role).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
    }

    fn session_id(s: &str) -> SessionId {
        SessionId::parse(s).unwrap()
    }

    #[test]
    fn agent_config_round_trips_defaults() {
        let cfg = AgentConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
        assert_eq!(back.auto_mode, AutoMode::Off);
        assert_eq!(back.role, Role::Reviewers);
        assert!(back.wfw_timeout.is_none());
        assert!(back.session.is_none());
    }

    #[test]
    fn agent_config_round_trips_populated() {
        let cfg = AgentConfig {
            auto_mode: AutoMode::Hint,
            role: Role::Master,
            wfw_timeout: Some("30m".into()),
            session: Some(Session {
                id: session_id("742f6a04-f174-409a-ab01-419a16c5f372"),
                tool: Tool::Claude,
                updated_at: "2026-05-23T16:24:47+10:00".into(),
            }),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn agent_config_accepts_missing_optional_fields() {
        let json = r#"{ "auto_mode": "wait" }"#;
        let cfg: AgentConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.auto_mode, AutoMode::Wait);
        assert_eq!(cfg.role, Role::Reviewers);
        assert!(cfg.wfw_timeout.is_none());
        assert!(cfg.session.is_none());
    }

    #[test]
    fn agent_config_omits_none_session() {
        let cfg = AgentConfig {
            auto_mode: AutoMode::Hint,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("session"), "expected no session key: {json}");
        assert!(
            !json.contains("wfw_timeout"),
            "expected no wfw_timeout key: {json}"
        );
    }

    #[test]
    fn role_for_returns_configured_role() {
        let cfg = AgentConfig {
            role: Role::Master,
            ..Default::default()
        };
        assert_eq!(role_for(&label("alice"), Some(&cfg)), Role::Master);
    }

    #[test]
    fn role_for_no_config_yields_default_reviewers() {
        assert_eq!(role_for(&label("alice"), None), Role::Reviewers);
    }
}
