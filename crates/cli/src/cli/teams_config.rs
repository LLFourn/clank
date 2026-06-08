//! Typed config schema for `teams-based-agent-registration`.
//!
//! New schema replacing the prior `agents`-array model from
//! `agents-declaration-is-user-local`. User-scope holds agent
//! DESCRIPTIONS + team COMPOSITIONS; repo-scope picks a team
//! (string or array) + optional `promoted` master override +
//! optional local entries.
//!
//! All structs round-trip through serde with
//! `#[derive(Deserialize, Serialize)]`. No whole-document
//! `serde_json::Value` parsing — only `extra:
//! BTreeMap<String, Value>` flatten catchalls preserve unknown
//! keys for forward-compat AND surface leftover keys (the
//! init migration uses `extra.contains_key("agents")` to
//! detect the legacy block).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use clank_core::agent_config::LaunchConfig;
use clank_core::ids::AgentLabel;
use clank_core::vocab::{Role, Tool};

/// User-scope `~/.clank/config.json`.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct UserConfigFile {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<AgentLabel, AgentDescription>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub teams: BTreeMap<String, TeamComposition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<crate::cli::config::ReviewSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<crate::cli::config::HooksSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<crate::cli::config::DiffConfig>,
    /// Forward-compat catchall. The `init` migration checks
    /// `extra.contains_key("agents")` to detect the legacy
    /// `default_agents` shape (when present).
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// One agent's description: which tool to spawn + optional
/// launch profile + optional initial prompt. NO role, NO team
/// affiliation — those are per-team properties.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentDescription {
    pub tool: Tool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<String>,
}

/// One team's composition: optional master + two reviewer
/// lists (commit-tier and gate-tier). `master` is optional at
/// the storage layer so `clank team create` can scaffold an
/// empty team; validation that master is set runs at
/// REGISTRATION-RESOLUTION time, not deserialize time.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TeamComposition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master: Option<AgentLabel>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commit_reviewers: Vec<AgentLabel>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gate_reviewers: Vec<AgentLabel>,
}

/// What kind of review a reviewer does. Names what they
/// review (commits vs gates) — not which abstract "tier" they
/// belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewKind {
    Commit,
    Gate,
}

/// Repo-scope `<repo>/.clank/config.json`. Gitignored by
/// `clank init` — config is per-user-per-repo.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct RepoConfigFile {
    /// String (sugar for `[{"include": "<name>"}]`) or array
    /// of `TeamEntry`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<TeamField>,
    /// Per-repo master designation. Written by
    /// `clank promote <agent>`. Doesn't touch the included
    /// team's user-scope master config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted: Option<AgentLabel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<crate::cli::config::ReviewSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<crate::cli::config::HooksSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<crate::cli::config::DiffConfig>,
    /// Forward-compat catchall. The `init` migration checks
    /// `extra.contains_key("agents")` to detect the legacy
    /// repo-scope `agents` block.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// The `team` field at repo scope: either a single string (a
/// team name, sugar for one-element array with `Include`) or
/// an array of mixed entries.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum TeamField {
    Single(String),
    Array(Vec<TeamEntry>),
}

/// One entry in the repo-scope `team` array. Three of the four
/// variants are non-include local additions; `Include` brings
/// in a user-scope team composition.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum TeamEntry {
    /// `{ "include": "<team-name>" }`.
    Include(IncludeEntry),
    /// `{ "agent": "<label>", "review": "..." }` — by-name
    /// reference with explicit review kind.
    ByName(ByNameEntry),
    /// `{ "label": "...", "tool": "...", "review": "...", ... }`
    /// — fully inline local agent.
    Inline(InlineAgent),
    /// Bare string: `"<label>"`. Equivalent to
    /// `{ "agent": "<label>" }` (review defaults to commit).
    BareString(AgentLabel),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IncludeEntry {
    pub include: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ByNameEntry {
    pub agent: AgentLabel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewKind>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InlineAgent {
    pub label: AgentLabel,
    pub tool: Tool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<String>,
    /// Optional explicit role. Validated AFTER deserialize:
    /// `Some(Role::Master)` is rejected with an actionable
    /// error pointing at `clank promote`. `None` and
    /// `Some(Role::Reviewer)` both resolve to reviewer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    /// What kind of review they do. Default `commit` if
    /// omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewKind>,
}

/// Legacy repo-scope config (the
/// `agents-declaration-is-user-local` shape) — used by
/// `clank init`'s migration fallback when new-format
/// deserialization fails entirely.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct LegacyRepoConfigFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<crate::cli::config::DefaultAgent>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<crate::cli::config::ReviewSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<crate::cli::config::HooksSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<crate::cli::config::DiffConfig>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
    }

    #[test]
    fn user_config_round_trips_full_schema() {
        let mut agents = BTreeMap::new();
        agents.insert(
            label("claude"),
            AgentDescription {
                tool: Tool::Claude,
                launch: None,
                initial_prompt: None,
            },
        );
        agents.insert(
            label("ruthless"),
            AgentDescription {
                tool: Tool::Claude,
                launch: Some(LaunchConfig {
                    command: Some("claude".into()),
                    args: vec!["--skill".into(), "ruthless".into()],
                    env: Default::default(),
                }),
                initial_prompt: None,
            },
        );
        let mut teams = BTreeMap::new();
        teams.insert(
            "default".to_string(),
            TeamComposition {
                master: Some(label("claude")),
                commit_reviewers: vec![label("codex")],
                gate_reviewers: vec![],
            },
        );
        teams.insert(
            "dev".to_string(),
            TeamComposition {
                master: Some(label("claude")),
                commit_reviewers: vec![label("codex")],
                gate_reviewers: vec![label("ruthless")],
            },
        );
        let cfg = UserConfigFile {
            agents,
            teams,
            ..Default::default()
        };
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let back: UserConfigFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agents.len(), 2);
        assert_eq!(back.teams.len(), 2);
        assert_eq!(
            back.teams
                .get("dev")
                .unwrap()
                .master
                .as_ref()
                .unwrap()
                .as_str(),
            "claude"
        );
        assert_eq!(
            back.teams.get("dev").unwrap().gate_reviewers[0].as_str(),
            "ruthless"
        );
    }

    #[test]
    fn team_composition_master_is_optional() {
        // Codex 4c79ed2 catch: empty-team scaffolding via
        // `clank team create` requires master to be storable
        // as None.
        let comp = TeamComposition::default();
        let json = serde_json::to_string(&comp).unwrap();
        let back: TeamComposition = serde_json::from_str(&json).unwrap();
        assert!(back.master.is_none());
        assert!(back.commit_reviewers.is_empty());
        assert!(back.gate_reviewers.is_empty());
        // Round-trip empty team JSON is just `{}`.
        assert_eq!(json, "{}");
    }

    #[test]
    fn repo_config_team_string_sugar_round_trips() {
        let cfg = RepoConfigFile {
            team: Some(TeamField::Single("dev".to_string())),
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        // The string form serializes as `"team": "dev"`.
        assert!(json.contains(r#""team":"dev""#));
        let back: RepoConfigFile = serde_json::from_str(&json).unwrap();
        match back.team {
            Some(TeamField::Single(s)) => assert_eq!(s, "dev"),
            other => panic!("expected Single; got {other:?}"),
        }
    }

    #[test]
    fn repo_config_team_array_round_trips_all_four_forms() {
        let json = r#"{
            "team": [
                { "include": "dev" },
                "ruthless",
                { "agent": "alice", "review": "gate" },
                { "label": "bob", "tool": "claude", "review": "commit" }
            ],
            "promoted": "codex"
        }"#;
        let cfg: RepoConfigFile = serde_json::from_str(json).unwrap();
        let entries = match cfg.team {
            Some(TeamField::Array(v)) => v,
            other => panic!("expected Array; got {other:?}"),
        };
        assert_eq!(entries.len(), 4);
        // Reserialize and parse again to confirm round-trip.
        let cfg2 = RepoConfigFile {
            team: Some(TeamField::Array(entries)),
            promoted: cfg.promoted.clone(),
            ..Default::default()
        };
        let s = serde_json::to_string(&cfg2).unwrap();
        let back: RepoConfigFile = serde_json::from_str(&s).unwrap();
        let entries2 = match back.team {
            Some(TeamField::Array(v)) => v,
            other => panic!("expected Array; got {other:?}"),
        };
        assert_eq!(entries2.len(), 4);
        assert_eq!(back.promoted.unwrap().as_str(), "codex");
    }

    #[test]
    fn repo_config_legacy_agents_key_lands_in_extra() {
        // Codex 4c79ed2 catch trigger #2: legacy agents block
        // in the JSON survives deserialization via `extra`
        // flatten — init's migration uses this to detect +
        // clean.
        let json = r#"{
            "team": "dev",
            "agents": [
                {"label": "alice", "role": "master", "tool": "claude"}
            ]
        }"#;
        let cfg: RepoConfigFile = serde_json::from_str(json).unwrap();
        // Team deserialized correctly.
        match cfg.team {
            Some(TeamField::Single(ref s)) if s == "dev" => {}
            other => panic!("expected Single(dev); got {other:?}"),
        }
        // Legacy agents key landed in extra (init migration
        // checks for this).
        assert!(cfg.extra.contains_key("agents"));
    }

    #[test]
    fn team_entry_bare_string_round_trips() {
        let entry = TeamEntry::BareString(label("ruthless"));
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(json, r#""ruthless""#);
        let back: TeamEntry = serde_json::from_str(&json).unwrap();
        match back {
            TeamEntry::BareString(l) => assert_eq!(l.as_str(), "ruthless"),
            other => panic!("expected BareString; got {other:?}"),
        }
    }

    #[test]
    fn team_entry_include_round_trips() {
        let entry = TeamEntry::Include(IncludeEntry {
            include: "dev".to_string(),
        });
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(json, r#"{"include":"dev"}"#);
        let back: TeamEntry = serde_json::from_str(&json).unwrap();
        match back {
            TeamEntry::Include(e) => assert_eq!(e.include, "dev"),
            other => panic!("expected Include; got {other:?}"),
        }
    }

    #[test]
    fn team_entry_by_name_round_trips() {
        let entry = TeamEntry::ByName(ByNameEntry {
            agent: label("alice"),
            review: Some(ReviewKind::Gate),
        });
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""agent":"alice""#));
        assert!(json.contains(r#""review":"gate""#));
        let back: TeamEntry = serde_json::from_str(&json).unwrap();
        match back {
            TeamEntry::ByName(e) => {
                assert_eq!(e.agent.as_str(), "alice");
                assert_eq!(e.review, Some(ReviewKind::Gate));
            }
            other => panic!("expected ByName; got {other:?}"),
        }
    }

    #[test]
    fn legacy_repo_config_parses_old_shape() {
        // Init's migration fallback path #1: pre-this-plan
        // repo config shape. Must deserialize via
        // LegacyRepoConfigFile when new-format deserialize
        // fails entirely.
        let json = r#"{
            "agents": [
                {"label": "alice", "role": "master", "tool": "claude"}
            ]
        }"#;
        let legacy: LegacyRepoConfigFile = serde_json::from_str(json).unwrap();
        let agents = legacy.agents.expect("agents block present");
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].label.as_str(), "alice");
    }
}
