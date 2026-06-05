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

use std::collections::BTreeMap;

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
    /// Per-agent launch profile for `clank agent start <label>`.
    /// `None` means "use the tool's bare name with no extra args
    /// or env." Populate to attach a skill / profile / env to
    /// this agent's spawner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchConfig>,
}

/// General-purpose launch profile: executable + args + env.
///
/// Two consumers in the codebase today (per OQ2 of
/// `clank-diff-editor`: reuse this struct rather than fork):
///
/// 1. **`clank agent start <name>`** (per-agent launch). Args are
///    spliced BEFORE the session-restore suffix (claude's
///    `--resume <id>` or codex's `resume <id> --cd <repo>`) so
///    they attach to the top-level tool. For codex specifically:
///    flags AFTER the `resume` subcommand attach to `resume`, not
///    to the `codex` binary; putting `launch.args` first preserves
///    the typical use case (`codex --profile deep resume <id>`).
///    For claude (flat flags) position is cosmetic, but the same
///    rule keeps the mental model consistent.
///
/// 2. **`clank diff` editor launch** (`Config.diff.editor`).
///    `args` may include template variables like `{range}`,
///    `{commits}`, `{patch_file}` that are substituted at compose
///    time. Args do not have a "session-restore suffix" concept;
///    args are passed verbatim (post-substitution) to the editor.
///
/// If future consumers need fields beyond `command/args/env`
/// (e.g. multi-pane layout descriptors), split into a sibling
/// struct rather than overloading this one.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LaunchConfig {
    /// Override the executable. `None` falls back to the
    /// session-tool's bare name (`claude` / `codex`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Args passed to the executable BEFORE the session-restore
    /// suffix. Example: `["--profile", "deep"]` for a codex agent
    /// produces `codex --profile deep resume <id> --cd <repo>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Env vars merged onto the calling process env. On key
    /// collision, this map wins ("config wins over inherited
    /// environment").
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
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
        assert_eq!(back.role, Role::Reviewer);
        assert!(back.wfw_timeout.is_none());
        assert!(back.session.is_none());
    }

    #[test]
    fn agent_config_round_trips_populated() {
        let cfg = AgentConfig {
            auto_mode: AutoMode::On,
            role: Role::Master,
            wfw_timeout: Some("30m".into()),
            session: Some(Session {
                id: session_id("742f6a04-f174-409a-ab01-419a16c5f372"),
                tool: Tool::Claude,
                updated_at: "2026-05-23T16:24:47+10:00".into(),
            }),
            launch: None,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn launch_field_deserializes_when_absent() {
        let json = r#"{ "auto_mode": "off" }"#;
        let cfg: AgentConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.launch.is_none());
        let back = serde_json::to_string(&cfg).unwrap();
        assert!(
            !back.contains("launch"),
            "Option::None must not serialize: {back}"
        );
    }

    #[test]
    fn launch_field_deserializes_when_present() {
        let json = r#"{
            "auto_mode": "off",
            "launch": {
                "command": "claude",
                "args": ["--skill", "ruthless"],
                "env": { "CLAUDE_PROFILE": "review" }
            }
        }"#;
        let cfg: AgentConfig = serde_json::from_str(json).unwrap();
        let launch = cfg.launch.expect("launch must deserialize");
        assert_eq!(launch.command.as_deref(), Some("claude"));
        assert_eq!(
            launch.args,
            vec!["--skill".to_string(), "ruthless".to_string()]
        );
        assert_eq!(
            launch.env.get("CLAUDE_PROFILE").map(|s| s.as_str()),
            Some("review")
        );
    }

    #[test]
    fn launch_config_default_is_empty() {
        let lc = LaunchConfig::default();
        assert!(lc.command.is_none());
        assert!(lc.args.is_empty());
        assert!(lc.env.is_empty());
        // Round-trips as `{}`.
        let json = serde_json::to_string(&lc).unwrap();
        assert_eq!(json, "{}", "default LaunchConfig should emit `{{}}`");
    }

    #[test]
    fn launch_config_round_trips_through_agent_config() {
        let mut env = BTreeMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        let cfg = AgentConfig {
            auto_mode: AutoMode::Off,
            role: Role::Reviewer,
            wfw_timeout: None,
            session: None,
            launch: Some(LaunchConfig {
                command: Some("codex".to_string()),
                args: vec!["--profile".to_string(), "deep".to_string()],
                env,
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
        assert_eq!(cfg.auto_mode, AutoMode::On);
        assert_eq!(cfg.role, Role::Reviewer);
        assert!(cfg.wfw_timeout.is_none());
        assert!(cfg.session.is_none());
    }

    #[test]
    fn agent_config_omits_none_session() {
        let cfg = AgentConfig {
            auto_mode: AutoMode::On,
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
        assert_eq!(role_for(&label("alice"), None), Role::Reviewer);
    }
}
