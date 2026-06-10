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
//! BTreeMap<String, Value>` flatten catchalls, which preserve
//! unknown keys for forward-compat (a leftover legacy
//! `agents`/`default_agents` key from a pre-hard-cut config
//! lands here and is ignored — there is no migration that reads
//! it).

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
/// `clank init` — config is per-user-per-repo.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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
    /// Forward-compat catchall. A leftover legacy repo-scope
    /// `agents` array (from a pre-hard-cut config) lands here and
    /// is ignored — there is no migration that reads it.
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
    #[error("repo config has no `team` field set; run `clank init --team <name>` to pick one")]
    NoTeamSet,
    #[error(
        "team `{0}` referenced from repo config is not declared in user-scope `~/.clank/config.json#/teams`"
    )]
    UnknownTeam(String),
    #[error(
        "agent `{0}` referenced from repo config is not declared in user-scope `~/.clank/config.json#/agents`"
    )]
    UnknownAgent(AgentLabel),
    #[error(
        "repo config `team` array contains more than one `include` entry (`{0}` and `{1}`); at most one is allowed in v1"
    )]
    MultipleIncludes(String, String),
    #[error(
        "inline local agent `{0}` has `role: \"master\"`, which is not allowed in `local_agents` entries. Use `clank promote {0}` after adding to designate master."
    )]
    InlineMasterRole(AgentLabel),
    #[error(
        "`promoted` label `{0}` is not present in the registered set built from `team` entries"
    )]
    PromotedNotPresent(AgentLabel),
    #[error(
        "team `{team}` has no master designated. Either set a team-level master via `clank team set-master {team} <agent>`, or designate a per-repo master via `clank promote <agent>`."
    )]
    NoMaster { team: String },
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

/// Resolve the registered set for a repo. Implements the
/// algorithm from the plan body's "Registration resolution"
/// section.
pub fn resolve_registered_set(
    user: &UserConfigFile,
    repo: &RepoConfigFile,
) -> Result<RegisteredSet, ResolutionError> {
    // Step 1: read `team`. None → no team set.
    let team_entries = match &repo.team {
        None => return Err(ResolutionError::NoTeamSet),
        Some(TeamField::Single(name)) => {
            // Sugar: single string → one Include entry.
            vec![TeamEntry::Include(IncludeEntry {
                include: name.clone(),
            })]
        }
        Some(TeamField::Array(v)) => v.clone(),
    };

    // Step 2: initialize accumulators.
    let mut master: Option<AgentLabel> = None;
    let mut included_team_name: Option<String> = None;
    let mut commit_reviewers: Vec<ResolvedAgent> = Vec::new();
    let mut gate_reviewers: Vec<ResolvedAgent> = Vec::new();

    // Track agents added (label → resolved descriptor) so
    // `promoted` can find them later AND so we can detect
    // same-label duplicates if they occur (out of scope to
    // reject here; the gate logic just needs `seen` for the
    // promoted lookup).
    fn add_to(list: &mut Vec<ResolvedAgent>, label: AgentLabel, desc: AgentDescription) {
        list.push(ResolvedAgent { label, desc });
    }
    fn lookup_user_agent<'a>(
        user: &'a UserConfigFile,
        label: &AgentLabel,
    ) -> Result<&'a AgentDescription, ResolutionError> {
        user.agents
            .get(label)
            .ok_or_else(|| ResolutionError::UnknownAgent(label.clone()))
    }

    // Step 3: walk team entries.
    for entry in team_entries {
        match entry {
            TeamEntry::Include(IncludeEntry { include: team_name }) => {
                if let Some(prev) = &included_team_name {
                    return Err(ResolutionError::MultipleIncludes(prev.clone(), team_name));
                }
                included_team_name = Some(team_name.clone());
                let team = user
                    .teams
                    .get(&team_name)
                    .ok_or(ResolutionError::UnknownTeam(team_name.clone()))?;
                if let Some(team_master_label) = &team.master {
                    let desc = lookup_user_agent(user, team_master_label)?;
                    master = Some(team_master_label.clone());
                    // For the typed return shape we also need
                    // the master's descriptor available later.
                    // We track it inline by saving the lookup
                    // here; the final RegisteredSet build uses
                    // `master_desc` from a second lookup.
                    let _ = desc;
                }
                for r in &team.commit_reviewers {
                    let d = lookup_user_agent(user, r)?;
                    add_to(&mut commit_reviewers, r.clone(), d.clone());
                }
                for r in &team.gate_reviewers {
                    let d = lookup_user_agent(user, r)?;
                    add_to(&mut gate_reviewers, r.clone(), d.clone());
                }
            }
            TeamEntry::BareString(label) => {
                let d = lookup_user_agent(user, &label)?;
                add_to(&mut commit_reviewers, label, d.clone());
            }
            TeamEntry::ByName(ByNameEntry { agent, review }) => {
                let d = lookup_user_agent(user, &agent)?;
                let list = match review.unwrap_or(ReviewKind::Commit) {
                    ReviewKind::Commit => &mut commit_reviewers,
                    ReviewKind::Gate => &mut gate_reviewers,
                };
                add_to(list, agent, d.clone());
            }
            TeamEntry::Inline(inline) => {
                if matches!(inline.role, Some(Role::Master)) {
                    return Err(ResolutionError::InlineMasterRole(inline.label));
                }
                let desc = AgentDescription {
                    tool: inline.tool,
                    launch: inline.launch,
                    initial_prompt: inline.initial_prompt,
                };
                let list = match inline.review.unwrap_or(ReviewKind::Commit) {
                    ReviewKind::Commit => &mut commit_reviewers,
                    ReviewKind::Gate => &mut gate_reviewers,
                };
                add_to(list, inline.label, desc);
            }
        }
    }

    // Step 4: apply `promoted` if set.
    // The master_desc accumulator carries the resolved
    // descriptor through to the RegisteredSet build below.
    // It's `Some(_)` if a master has been chosen at any
    // point in steps 3-4; the codex 98ed204 catch fixed
    // here uses the inline agent's descriptor when promoted
    // points at an inline-only local (the previous code
    // discarded that descriptor and re-looked-up from
    // user-scope, which failed UnknownAgent for inline-only
    // promoted agents).
    let mut master_desc: Option<AgentDescription> = None;
    if let Some(promoted_label) = &repo.promoted {
        // Find new_master in commit_reviewers or gate_reviewers.
        let from_commit = commit_reviewers
            .iter()
            .position(|a| &a.label == promoted_label);
        let from_gate = gate_reviewers
            .iter()
            .position(|a| &a.label == promoted_label);
        let new_master_desc: AgentDescription = if let Some(i) = from_commit {
            commit_reviewers.remove(i).desc
        } else if let Some(i) = from_gate {
            gate_reviewers.remove(i).desc
        } else if master.as_ref() == Some(promoted_label) {
            // promoted == existing master → no-op. Resolve
            // from user-scope (existing master always is).
            user.agents
                .get(promoted_label)
                .cloned()
                .ok_or_else(|| ResolutionError::UnknownAgent(promoted_label.clone()))?
        } else {
            return Err(ResolutionError::PromotedNotPresent(promoted_label.clone()));
        };
        // Demote previous master to commit_reviewers (if it
        // differed from promoted and existed in user-scope).
        if let Some(prev_master_label) = master.take() {
            if &prev_master_label != promoted_label {
                let prev_desc = user
                    .agents
                    .get(&prev_master_label)
                    .cloned()
                    .ok_or_else(|| ResolutionError::UnknownAgent(prev_master_label.clone()))?;
                commit_reviewers.push(ResolvedAgent {
                    label: prev_master_label,
                    desc: prev_desc,
                });
            }
        }
        master = Some(promoted_label.clone());
        master_desc = Some(new_master_desc);
    }

    // Step 5: validate master is Some.
    let master_label = master.ok_or_else(|| ResolutionError::NoMaster {
        team: included_team_name.unwrap_or_else(|| "<no team included>".to_string()),
    })?;
    // master_desc was set by step 4 when promoted fired,
    // or comes from user-scope agents for the
    // team-included master.
    let master_desc = match master_desc {
        Some(d) => d,
        None => user
            .agents
            .get(&master_label)
            .cloned()
            .ok_or_else(|| ResolutionError::UnknownAgent(master_label.clone()))?,
    };

    // Step 6: validate registered-set integrity (codex
    // 98ed204 pin):
    // - Master must NOT also be in either reviewer list.
    // - The same label must NOT appear in BOTH reviewer
    //   lists (one tier per registered agent).
    // The plan body pins both at parse-time / registration-
    // time; checking once after resolution makes the gate
    // logic downstream rely on a clean shape.
    if commit_reviewers.iter().any(|a| a.label == master_label) {
        return Err(ResolutionError::MasterInReviewerList(master_label));
    }
    if gate_reviewers.iter().any(|a| a.label == master_label) {
        return Err(ResolutionError::MasterInReviewerList(master_label));
    }
    for ca in &commit_reviewers {
        if gate_reviewers.iter().any(|ga| ga.label == ca.label) {
            return Err(ResolutionError::ReviewerInBothLists(ca.label.clone()));
        }
    }
    // Duplicate label within the same reviewer list (two
    // entries for the same agent in commit_reviewers, etc.)
    // is also rejected — registration is a set, not a bag.
    fn first_duplicate(list: &[ResolvedAgent]) -> Option<AgentLabel> {
        for (i, a) in list.iter().enumerate() {
            for b in list.iter().skip(i + 1) {
                if a.label == b.label {
                    return Some(a.label.clone());
                }
            }
        }
        None
    }
    if let Some(dup) = first_duplicate(&commit_reviewers) {
        return Err(ResolutionError::DuplicateReviewer(dup));
    }
    if let Some(dup) = first_duplicate(&gate_reviewers) {
        return Err(ResolutionError::DuplicateReviewer(dup));
    }

    Ok(RegisteredSet {
        master: master_label,
        master_desc,
        commit_reviewers,
        gate_reviewers,
    })
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

    // ── resolve_registered_set ───────────────────────────

    fn user_with(
        agents: Vec<(&str, Tool)>,
        teams: Vec<(&str, Option<&str>, Vec<&str>, Vec<&str>)>,
    ) -> UserConfigFile {
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
        let mut team_map = BTreeMap::new();
        for (name, master, commit, gate) in teams {
            team_map.insert(
                name.to_string(),
                TeamComposition {
                    master: master.map(label),
                    commit_reviewers: commit.into_iter().map(label).collect(),
                    gate_reviewers: gate.into_iter().map(label).collect(),
                },
            );
        }
        UserConfigFile {
            agents: agent_map,
            teams: team_map,
            ..Default::default()
        }
    }

    #[test]
    fn resolve_with_single_team_string() {
        let user = user_with(
            vec![("claude", Tool::Claude), ("codex", Tool::Codex)],
            vec![("dev", Some("claude"), vec!["codex"], vec![])],
        );
        let repo = RepoConfigFile {
            team: Some(TeamField::Single("dev".to_string())),
            ..Default::default()
        };
        let r = resolve_registered_set(&user, &repo).unwrap();
        assert_eq!(r.master.as_str(), "claude");
        assert_eq!(r.commit_reviewers.len(), 1);
        assert_eq!(r.commit_reviewers[0].label.as_str(), "codex");
        assert!(r.gate_reviewers.is_empty());
    }

    #[test]
    fn resolve_with_gate_reviewers() {
        let user = user_with(
            vec![
                ("claude", Tool::Claude),
                ("codex", Tool::Codex),
                ("ruthless", Tool::Claude),
            ],
            vec![("dev", Some("claude"), vec!["codex"], vec!["ruthless"])],
        );
        let repo = RepoConfigFile {
            team: Some(TeamField::Single("dev".to_string())),
            ..Default::default()
        };
        let r = resolve_registered_set(&user, &repo).unwrap();
        assert_eq!(r.master.as_str(), "claude");
        assert_eq!(r.commit_reviewers.len(), 1);
        assert_eq!(r.gate_reviewers.len(), 1);
        assert_eq!(r.gate_reviewers[0].label.as_str(), "ruthless");
    }

    #[test]
    fn resolve_promoted_swaps_master() {
        // Codex 4c79ed2 catch + swap semantic: promoted moves
        // to master; previous master demotes to commit_reviewers
        // (not dropped).
        let user = user_with(
            vec![("claude", Tool::Claude), ("codex", Tool::Codex)],
            vec![("dev", Some("claude"), vec!["codex"], vec![])],
        );
        let repo = RepoConfigFile {
            team: Some(TeamField::Single("dev".to_string())),
            promoted: Some(label("codex")),
            ..Default::default()
        };
        let r = resolve_registered_set(&user, &repo).unwrap();
        assert_eq!(r.master.as_str(), "codex");
        // claude (prev master) demoted to commit_reviewers.
        let cr_labels: Vec<_> = r
            .commit_reviewers
            .iter()
            .map(|a| a.label.as_str())
            .collect();
        assert!(cr_labels.contains(&"claude"));
        // codex no longer in reviewer lists.
        assert!(!cr_labels.contains(&"codex"));
    }

    #[test]
    fn resolve_local_entries_via_array_form() {
        let user = user_with(
            vec![
                ("claude", Tool::Claude),
                ("codex", Tool::Codex),
                ("ruthless", Tool::Claude),
                ("alice", Tool::Claude),
            ],
            vec![("dev", Some("claude"), vec!["codex"], vec![])],
        );
        let repo = RepoConfigFile {
            team: Some(TeamField::Array(vec![
                TeamEntry::Include(IncludeEntry {
                    include: "dev".to_string(),
                }),
                TeamEntry::BareString(label("ruthless")),
                TeamEntry::ByName(ByNameEntry {
                    agent: label("alice"),
                    review: Some(ReviewKind::Gate),
                }),
            ])),
            ..Default::default()
        };
        let r = resolve_registered_set(&user, &repo).unwrap();
        let cr: Vec<_> = r
            .commit_reviewers
            .iter()
            .map(|a| a.label.as_str())
            .collect();
        let gr: Vec<_> = r.gate_reviewers.iter().map(|a| a.label.as_str()).collect();
        assert!(cr.contains(&"codex"));
        assert!(cr.contains(&"ruthless"));
        assert!(gr.contains(&"alice"));
    }

    #[test]
    fn resolve_no_team_set_errors() {
        let user = UserConfigFile::default();
        let repo = RepoConfigFile::default();
        let err = resolve_registered_set(&user, &repo).unwrap_err();
        assert!(matches!(err, ResolutionError::NoTeamSet));
    }

    #[test]
    fn resolve_unknown_team_errors() {
        let user = UserConfigFile::default();
        let repo = RepoConfigFile {
            team: Some(TeamField::Single("nope".to_string())),
            ..Default::default()
        };
        let err = resolve_registered_set(&user, &repo).unwrap_err();
        match err {
            ResolutionError::UnknownTeam(s) => assert_eq!(s, "nope"),
            other => panic!("expected UnknownTeam; got {other:?}"),
        }
    }

    #[test]
    fn resolve_no_master_errors_with_actionable_hint() {
        // Codex 4c79ed2 catch: team scaffolded via
        // `clank team create` with no master, and no
        // `promoted` at repo scope → error at resolution
        // time, NOT deserialize time.
        let user = user_with(
            vec![("codex", Tool::Codex)],
            vec![("dev", None, vec!["codex"], vec![])],
        );
        let repo = RepoConfigFile {
            team: Some(TeamField::Single("dev".to_string())),
            ..Default::default()
        };
        let err = resolve_registered_set(&user, &repo).unwrap_err();
        match err {
            ResolutionError::NoMaster { team } => assert_eq!(team, "dev"),
            other => panic!("expected NoMaster; got {other:?}"),
        }
        // Error message mentions both fix paths.
        let msg = resolve_registered_set(&user, &repo)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("clank team set-master"));
        assert!(msg.contains("clank promote"));
    }

    #[test]
    fn resolve_no_master_team_recoverable_via_promoted() {
        // Same setup but `promoted` at repo scope provides
        // the master. Resolution succeeds.
        let user = user_with(
            vec![("codex", Tool::Codex)],
            vec![("dev", None, vec!["codex"], vec![])],
        );
        let repo = RepoConfigFile {
            team: Some(TeamField::Single("dev".to_string())),
            promoted: Some(label("codex")),
            ..Default::default()
        };
        let r = resolve_registered_set(&user, &repo).unwrap();
        assert_eq!(r.master.as_str(), "codex");
        assert!(r.commit_reviewers.is_empty());
    }

    #[test]
    fn resolve_multiple_includes_rejected() {
        let user = user_with(
            vec![("claude", Tool::Claude), ("grok", Tool::Claude)],
            vec![
                ("dev", Some("claude"), vec![], vec![]),
                ("research", Some("grok"), vec![], vec![]),
            ],
        );
        let repo = RepoConfigFile {
            team: Some(TeamField::Array(vec![
                TeamEntry::Include(IncludeEntry {
                    include: "dev".to_string(),
                }),
                TeamEntry::Include(IncludeEntry {
                    include: "research".to_string(),
                }),
            ])),
            ..Default::default()
        };
        let err = resolve_registered_set(&user, &repo).unwrap_err();
        match err {
            ResolutionError::MultipleIncludes(a, b) => {
                assert_eq!(a, "dev");
                assert_eq!(b, "research");
            }
            other => panic!("expected MultipleIncludes; got {other:?}"),
        }
    }

    #[test]
    fn resolve_inline_master_role_rejected() {
        let user = user_with(
            vec![("claude", Tool::Claude)],
            vec![("dev", Some("claude"), vec![], vec![])],
        );
        let repo = RepoConfigFile {
            team: Some(TeamField::Array(vec![
                TeamEntry::Include(IncludeEntry {
                    include: "dev".to_string(),
                }),
                TeamEntry::Inline(InlineAgent {
                    label: label("alice"),
                    tool: Tool::Claude,
                    launch: None,
                    initial_prompt: None,
                    role: Some(Role::Master),
                    review: None,
                }),
            ])),
            ..Default::default()
        };
        let err = resolve_registered_set(&user, &repo).unwrap_err();
        match err {
            ResolutionError::InlineMasterRole(l) => assert_eq!(l.as_str(), "alice"),
            other => panic!("expected InlineMasterRole; got {other:?}"),
        }
    }

    #[test]
    fn resolve_promoted_not_present_rejected() {
        let user = user_with(
            vec![("claude", Tool::Claude), ("codex", Tool::Codex)],
            vec![("dev", Some("claude"), vec!["codex"], vec![])],
        );
        let repo = RepoConfigFile {
            team: Some(TeamField::Single("dev".to_string())),
            promoted: Some(label("phantom")),
            ..Default::default()
        };
        let err = resolve_registered_set(&user, &repo).unwrap_err();
        match err {
            ResolutionError::PromotedNotPresent(l) => assert_eq!(l.as_str(), "phantom"),
            other => panic!("expected PromotedNotPresent; got {other:?}"),
        }
    }

    #[test]
    fn resolve_promoted_works_for_inline_only_local_agent() {
        // Codex 98ed204 catch: inline-only agents (not
        // declared in user-scope `agents`) must still be
        // promotable. Pre-fix the resolver re-looked-up
        // master_desc from user-scope and failed
        // UnknownAgent when the promoted target was inline.
        let user = user_with(
            vec![("claude", Tool::Claude), ("codex", Tool::Codex)],
            vec![("dev", Some("claude"), vec!["codex"], vec![])],
        );
        let repo = RepoConfigFile {
            team: Some(TeamField::Array(vec![
                TeamEntry::Include(IncludeEntry {
                    include: "dev".to_string(),
                }),
                TeamEntry::Inline(InlineAgent {
                    label: label("alice"),
                    tool: Tool::Claude,
                    launch: None,
                    initial_prompt: None,
                    role: None,
                    review: None,
                }),
            ])),
            promoted: Some(label("alice")),
            ..Default::default()
        };
        let r = resolve_registered_set(&user, &repo).unwrap();
        assert_eq!(r.master.as_str(), "alice");
        assert_eq!(r.master_desc.tool, Tool::Claude);
        // claude (prev master) joined commit_reviewers.
        let cr_labels: Vec<_> = r
            .commit_reviewers
            .iter()
            .map(|a| a.label.as_str())
            .collect();
        assert!(cr_labels.contains(&"claude"));
        assert!(cr_labels.contains(&"codex"));
        assert!(!cr_labels.contains(&"alice"));
    }

    #[test]
    fn resolve_rejects_master_also_in_reviewer_list() {
        // Codex 98ed204 catch: validate registered-set
        // integrity AFTER resolution.
        let user = user_with(
            vec![("claude", Tool::Claude), ("codex", Tool::Codex)],
            // Team itself declares claude as both master AND
            // commit_reviewer — invalid but presents to the
            // resolver as such because user authored the
            // user-scope config by hand.
            vec![("dev", Some("claude"), vec!["claude"], vec![])],
        );
        let repo = RepoConfigFile {
            team: Some(TeamField::Single("dev".to_string())),
            ..Default::default()
        };
        let err = resolve_registered_set(&user, &repo).unwrap_err();
        match err {
            ResolutionError::MasterInReviewerList(l) => assert_eq!(l.as_str(), "claude"),
            other => panic!("expected MasterInReviewerList; got {other:?}"),
        }
    }

    #[test]
    fn resolve_rejects_reviewer_in_both_tier_lists() {
        // Codex 98ed204 catch: an agent in both
        // commit_reviewers and gate_reviewers is invalid.
        // Same defense-in-depth as MasterInReviewerList.
        let user = user_with(
            vec![("claude", Tool::Claude), ("codex", Tool::Codex)],
            vec![("dev", Some("claude"), vec!["codex"], vec!["codex"])],
        );
        let repo = RepoConfigFile {
            team: Some(TeamField::Single("dev".to_string())),
            ..Default::default()
        };
        let err = resolve_registered_set(&user, &repo).unwrap_err();
        match err {
            ResolutionError::ReviewerInBothLists(l) => assert_eq!(l.as_str(), "codex"),
            other => panic!("expected ReviewerInBothLists; got {other:?}"),
        }
    }

    #[test]
    fn resolve_rejects_duplicate_reviewer_within_same_list() {
        // Defense for the same-label-twice case via local
        // entries: bare-string "codex" added to a team that
        // already lists codex in commit_reviewers.
        let user = user_with(
            vec![("claude", Tool::Claude), ("codex", Tool::Codex)],
            vec![("dev", Some("claude"), vec!["codex"], vec![])],
        );
        let repo = RepoConfigFile {
            team: Some(TeamField::Array(vec![
                TeamEntry::Include(IncludeEntry {
                    include: "dev".to_string(),
                }),
                TeamEntry::BareString(label("codex")),
            ])),
            ..Default::default()
        };
        let err = resolve_registered_set(&user, &repo).unwrap_err();
        match err {
            ResolutionError::DuplicateReviewer(l) => assert_eq!(l.as_str(), "codex"),
            other => panic!("expected DuplicateReviewer; got {other:?}"),
        }
    }
}
