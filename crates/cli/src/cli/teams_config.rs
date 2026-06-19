//! Typed config schema for `teams-based-agent-registration`.
//!
//! User-scope holds agent DESCRIPTIONS + team COMPOSITIONS.
//! Repo-scope reuses the same building blocks: repo-local agent
//! DESCRIPTIONS plus exactly one [`TeamComposition`] (by-name
//! refs into the repo's own `agents`). The repo config is
//! self-contained — resolving its registered set needs no
//! user-scope template.
//!
//! All structs round-trip through serde with
//! `#[derive(Deserialize, Serialize)]`. No whole-document
//! `serde_json::Value` parsing — only `extra:
//! BTreeMap<String, Value>` flatten catchalls, which preserve
//! unknown keys for forward-compat (a leftover legacy
//! `agents`/`default_agents` key from a pre-hard-cut config
//! lands here and is ignored — there is no migration that reads
//! it).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use clank_core::agent_config::LaunchConfig;
use clank_core::ids::AgentLabel;
use clank_core::vocab::{AutoMode, Tool};

/// User-scope `~/.clank/config.json`.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct UserConfigFile {
    /// Machine-wide default auto-mode (`auto-mode-default-on`). A
    /// fresh agent config inherits this when it has no explicit
    /// `clank auto on|off`; absent = Off (other machines unaffected
    /// until they opt in). Resolved via
    /// `clank_core::agent_config::effective_auto_mode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<AutoMode>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zellij: Option<ZellijSection>,
    /// Forward-compat catchall. A leftover legacy `default_agents`
    /// key (from a pre-hard-cut config) lands here and is ignored
    /// — there is no migration that reads it.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// User-scope zellij preferences
/// (`zellij-layout-config-around-agent-panes`). UI chrome is a
/// personal preference, so this lives in `~/.clank/config.json`
/// only — no repo-scope override.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ZellijSection {
    /// KDL layout TEMPLATE wrapping clank's agent panes. Must
    /// contain a `clank_agents` marker node, which clank replaces
    /// with the composed agent pane group — master stage + stacked
    /// reviewers + a `clank status --tui` instrument pane (the
    /// group BRINGS its own status pane; don't add another). Unset
    /// → clank's built-in layout. Example with custom chrome:
    ///
    /// ```kdl
    /// layout {
    ///     default_tab_template {
    ///         pane size=1 borderless=true { plugin location="compact-bar" }
    ///         children
    ///     }
    ///     tab name="clank" {
    ///         clank_agents
    ///     }
    /// }
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
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
/// `clank init` — config is per-user-per-repo. Reuses the same
/// building blocks as [`UserConfigFile`]: repo-local agent
/// DESCRIPTIONS plus exactly one team composition (by-name refs
/// into `agents`). No user-scope template is required — the repo
/// is self-contained.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RepoConfigFile {
    /// Repo-local agent descriptions. The team's master +
    /// reviewers reference labels in this map by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<AgentLabel, AgentDescription>,
    /// This repo's single team composition. Empty
    /// (master `None`, empty reviewer lists) on a freshly
    /// bootstrapped repo.
    #[serde(default, skip_serializing_if = "is_default_team")]
    pub team: TeamComposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<crate::cli::config::ReviewSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<crate::cli::config::HooksSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<crate::cli::config::DiffConfig>,
    /// Forward-compat catchall. A leftover legacy repo-scope
    /// `agents` array (from a pre-hard-cut config) lands here and
    /// is ignored — there is no migration that reads it.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn is_default_team(team: &TeamComposition) -> bool {
    team.master.is_none() && team.commit_reviewers.is_empty() && team.gate_reviewers.is_empty()
}

/// Resolved registered set for a repo: master + two reviewer
/// lists. Each agent has both a label and the description
/// resolved at registration time.
#[derive(Debug, Clone)]
pub struct RegisteredSet {
    pub master: AgentLabel,
    pub master_desc: AgentDescription,
    pub commit_reviewers: Vec<ResolvedAgent>,
    pub gate_reviewers: Vec<ResolvedAgent>,
}

#[derive(Debug, Clone)]
pub struct ResolvedAgent {
    pub label: AgentLabel,
    pub desc: AgentDescription,
}

/// Errors raised while resolving the registered set for a
/// repo. Each variant names the user-facing problem so the
/// CLI can map to actionable diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum ResolutionError {
    #[error("no master set; run `clank team set-master <agent>`")]
    NoMaster,
    #[error("agent `{0}` is referenced by the team but not defined in this repo's `agents`")]
    UnknownAgent(AgentLabel),
    #[error(
        "agent `{0}` is the team's master AND listed as a reviewer; an agent has one role per team"
    )]
    MasterInReviewerList(AgentLabel),
    #[error(
        "agent `{0}` appears in BOTH `commit_reviewers` and `gate_reviewers`; an agent has one review kind per team"
    )]
    ReviewerInBothLists(AgentLabel),
    #[error(
        "agent `{0}` appears more than once in the same reviewer list; registration is a set, not a bag"
    )]
    DuplicateReviewer(AgentLabel),
}

/// Resolve the registered set for a repo. The repo is
/// self-contained: master + reviewers are by-name references
/// into the repo's own `agents` map.
pub fn resolve_registered_set(repo: &RepoConfigFile) -> Result<RegisteredSet, ResolutionError> {
    let lookup = |label: &AgentLabel| -> Result<AgentDescription, ResolutionError> {
        repo.agents
            .get(label)
            .cloned()
            .ok_or_else(|| ResolutionError::UnknownAgent(label.clone()))
    };

    let master_label = repo.team.master.clone().ok_or(ResolutionError::NoMaster)?;
    let master_desc = lookup(&master_label)?;

    // Master must not also be a reviewer.
    if repo.team.commit_reviewers.contains(&master_label)
        || repo.team.gate_reviewers.contains(&master_label)
    {
        return Err(ResolutionError::MasterInReviewerList(master_label));
    }

    // No duplicate within a single reviewer list.
    if let Some(dup) = first_duplicate(&repo.team.commit_reviewers) {
        return Err(ResolutionError::DuplicateReviewer(dup));
    }
    if let Some(dup) = first_duplicate(&repo.team.gate_reviewers) {
        return Err(ResolutionError::DuplicateReviewer(dup));
    }

    // No agent in BOTH reviewer lists.
    for c in &repo.team.commit_reviewers {
        if repo.team.gate_reviewers.contains(c) {
            return Err(ResolutionError::ReviewerInBothLists(c.clone()));
        }
    }

    let mut commit_reviewers = Vec::with_capacity(repo.team.commit_reviewers.len());
    for label in &repo.team.commit_reviewers {
        commit_reviewers.push(ResolvedAgent {
            label: label.clone(),
            desc: lookup(label)?,
        });
    }
    let mut gate_reviewers = Vec::with_capacity(repo.team.gate_reviewers.len());
    for label in &repo.team.gate_reviewers {
        gate_reviewers.push(ResolvedAgent {
            label: label.clone(),
            desc: lookup(label)?,
        });
    }

    Ok(RegisteredSet {
        master: master_label,
        master_desc,
        commit_reviewers,
        gate_reviewers,
    })
}

fn first_duplicate(list: &[AgentLabel]) -> Option<AgentLabel> {
    for (i, a) in list.iter().enumerate() {
        if list.iter().skip(i + 1).any(|b| b == a) {
            return Some(a.clone());
        }
    }
    None
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
    fn repo_config_empty_round_trips() {
        // A bootstrapped repo (empty agents, default team)
        // serializes to `{}` and parses back to empty.
        let cfg = RepoConfigFile::default();
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(json, "{}");
        let back: RepoConfigFile = serde_json::from_str(&json).unwrap();
        assert!(back.agents.is_empty());
        assert!(back.team.master.is_none());
        assert!(back.team.commit_reviewers.is_empty());
        assert!(back.team.gate_reviewers.is_empty());
    }

    #[test]
    fn repo_config_full_round_trips() {
        let cfg = repo_with(
            vec![
                ("claude", Tool::Claude),
                ("codex", Tool::Codex),
                ("ruthless", Tool::Claude),
            ],
            Some("claude"),
            vec!["codex"],
            vec!["ruthless"],
        );
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let back: RepoConfigFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agents.len(), 3);
        assert_eq!(back.team.master.as_ref().unwrap().as_str(), "claude");
        assert_eq!(back.team.commit_reviewers[0].as_str(), "codex");
        assert_eq!(back.team.gate_reviewers[0].as_str(), "ruthless");
    }

    // ── resolve_registered_set ───────────────────────────

    fn repo_with(
        agents: Vec<(&str, Tool)>,
        master: Option<&str>,
        commit: Vec<&str>,
        gate: Vec<&str>,
    ) -> RepoConfigFile {
        let mut agent_map = BTreeMap::new();
        for (l, t) in agents {
            agent_map.insert(
                label(l),
                AgentDescription {
                    tool: t,
                    launch: None,
                    initial_prompt: None,
                },
            );
        }
        RepoConfigFile {
            agents: agent_map,
            team: TeamComposition {
                master: master.map(label),
                commit_reviewers: commit.into_iter().map(label).collect(),
                gate_reviewers: gate.into_iter().map(label).collect(),
            },
            ..Default::default()
        }
    }

    #[test]
    fn resolve_master_and_commit_reviewer() {
        let repo = repo_with(
            vec![("claude", Tool::Claude), ("codex", Tool::Codex)],
            Some("claude"),
            vec!["codex"],
            vec![],
        );
        let r = resolve_registered_set(&repo).unwrap();
        assert_eq!(r.master.as_str(), "claude");
        assert_eq!(r.master_desc.tool, Tool::Claude);
        assert_eq!(r.commit_reviewers.len(), 1);
        assert_eq!(r.commit_reviewers[0].label.as_str(), "codex");
        assert_eq!(r.commit_reviewers[0].desc.tool, Tool::Codex);
        assert!(r.gate_reviewers.is_empty());
    }

    #[test]
    fn resolve_with_gate_reviewers() {
        let repo = repo_with(
            vec![
                ("claude", Tool::Claude),
                ("codex", Tool::Codex),
                ("ruthless", Tool::Claude),
            ],
            Some("claude"),
            vec!["codex"],
            vec!["ruthless"],
        );
        let r = resolve_registered_set(&repo).unwrap();
        assert_eq!(r.master.as_str(), "claude");
        assert_eq!(r.commit_reviewers.len(), 1);
        assert_eq!(r.gate_reviewers.len(), 1);
        assert_eq!(r.gate_reviewers[0].label.as_str(), "ruthless");
    }

    #[test]
    fn resolve_empty_bootstrapped_repo_errors_no_master() {
        // A freshly-bootstrapped repo: empty agents, default
        // (empty) team → NoMaster with the actionable hint.
        let repo = RepoConfigFile::default();
        let err = resolve_registered_set(&repo).unwrap_err();
        assert!(matches!(err, ResolutionError::NoMaster));
        let msg = err.to_string();
        assert!(msg.contains("clank team set-master"));
        // NoMaster must not mention teams-by-name anymore.
        assert!(!msg.contains("set-master <team>"));
    }

    #[test]
    fn resolve_unknown_master_agent_errors() {
        // Master references an agent not defined in `agents`.
        let repo = repo_with(vec![], Some("claude"), vec![], vec![]);
        let err = resolve_registered_set(&repo).unwrap_err();
        match err {
            ResolutionError::UnknownAgent(l) => assert_eq!(l.as_str(), "claude"),
            other => panic!("expected UnknownAgent; got {other:?}"),
        }
    }

    #[test]
    fn resolve_unknown_reviewer_agent_errors() {
        // Reviewer references an agent not defined in `agents`.
        let repo = repo_with(
            vec![("claude", Tool::Claude)],
            Some("claude"),
            vec!["codex"],
            vec![],
        );
        let err = resolve_registered_set(&repo).unwrap_err();
        match err {
            ResolutionError::UnknownAgent(l) => assert_eq!(l.as_str(), "codex"),
            other => panic!("expected UnknownAgent; got {other:?}"),
        }
    }

    #[test]
    fn resolve_rejects_master_also_in_reviewer_list() {
        let repo = repo_with(
            vec![("claude", Tool::Claude)],
            Some("claude"),
            vec!["claude"],
            vec![],
        );
        let err = resolve_registered_set(&repo).unwrap_err();
        match err {
            ResolutionError::MasterInReviewerList(l) => assert_eq!(l.as_str(), "claude"),
            other => panic!("expected MasterInReviewerList; got {other:?}"),
        }
    }

    #[test]
    fn resolve_rejects_reviewer_in_both_tier_lists() {
        let repo = repo_with(
            vec![("claude", Tool::Claude), ("codex", Tool::Codex)],
            Some("claude"),
            vec!["codex"],
            vec!["codex"],
        );
        let err = resolve_registered_set(&repo).unwrap_err();
        match err {
            ResolutionError::ReviewerInBothLists(l) => assert_eq!(l.as_str(), "codex"),
            other => panic!("expected ReviewerInBothLists; got {other:?}"),
        }
    }

    #[test]
    fn resolve_rejects_duplicate_reviewer_within_same_list() {
        let repo = repo_with(
            vec![("claude", Tool::Claude), ("codex", Tool::Codex)],
            Some("claude"),
            vec!["codex", "codex"],
            vec![],
        );
        let err = resolve_registered_set(&repo).unwrap_err();
        match err {
            ResolutionError::DuplicateReviewer(l) => assert_eq!(l.as_str(), "codex"),
            other => panic!("expected DuplicateReviewer; got {other:?}"),
        }
    }
}
