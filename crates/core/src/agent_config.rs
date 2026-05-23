//! Typed config schemas for per-agent and repo-level Clank state.
//!
//! Pure data + serde round-trip. No clock, no env, no filesystem
//! — the CLI is the only place that touches the world. These
//! types just describe what's written to:
//! - `.clank/agents/<label>/config.json` — per-agent, per-machine
//!   ([`AgentConfig`]). Gitignored.
//! - `.clank/config.json` — repo-shared ([`RepoConfig`]). Tracked.

use serde::{Deserialize, Serialize};

use crate::ids::{AgentLabel, SessionId};
use crate::vocab::{AutoMode, Role, Tool};

/// Per-agent state for one (label, repo). Lives at
/// `.clank/agents/<label>/config.json`. Gitignored — auto-mode
/// preferences and session bindings are per-machine concerns.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub auto_mode: AutoMode,
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
    /// RFC3339 / ISO-8601 timestamp of the last bind, e.g.
    /// `"2026-05-23T16:24:47+10:00"`. Stringly typed so core stays
    /// free of `chrono`/`time` deps; the CLI formats with whatever
    /// clock source it has.
    pub updated_at: String,
}

/// Repo-shared settings. Lives at `.clank/config.json`. Tracked.
/// Only field in v1 is `master`; schema is extensible via
/// additional optional `serde` fields.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RepoConfig {
    /// Agent label designated as master for this repo. `None`
    /// means no master is set; operations that need one (e.g.
    /// wfw inferring `--role`) default to `reviewers`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master: Option<AgentLabel>,
}

/// Role an agent plays for this repo: master iff its label
/// matches `config.master`, reviewers otherwise.
///
/// Pure helper — no I/O, no clock. Caller loads `RepoConfig` from
/// disk (or passes `None` if missing) and the agent's label
/// (resolved through the identity resolver) and gets back the role.
pub fn role_for(label: &AgentLabel, config: Option<&RepoConfig>) -> Role {
    match config.and_then(|c| c.master.as_ref()) {
        Some(master) if master == label => Role::Master,
        _ => Role::Reviewers,
    }
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
        assert!(back.wfw_timeout.is_none());
        assert!(back.session.is_none());
    }

    #[test]
    fn agent_config_round_trips_populated() {
        let cfg = AgentConfig {
            auto_mode: AutoMode::Hint,
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
    fn repo_config_round_trips() {
        let cfg = RepoConfig {
            master: Some(label("alice")),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(json, r#"{"master":"alice"}"#);
        let back: RepoConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn repo_config_empty_omits_master() {
        let cfg = RepoConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(json, "{}");
    }

    #[test]
    fn role_for_matches_master_label() {
        let cfg = RepoConfig {
            master: Some(label("alice")),
        };
        assert_eq!(role_for(&label("alice"), Some(&cfg)), Role::Master);
        assert_eq!(role_for(&label("bob"), Some(&cfg)), Role::Reviewers);
    }

    #[test]
    fn role_for_no_master_yields_reviewers() {
        assert_eq!(role_for(&label("alice"), None), Role::Reviewers);
        assert_eq!(
            role_for(&label("alice"), Some(&RepoConfig::default())),
            Role::Reviewers
        );
    }
}
