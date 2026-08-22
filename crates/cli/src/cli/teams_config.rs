//! Typed config schema for `repo-agents-no-team`.
//!
//! The REPO config is a flat ROSTER: a `BTreeMap<AgentLabel,
//! RosterAgent>` where each agent carries its DEFINITION (tool /
//! launch / initial_prompt) AND its ROLE (master / commit / gate).
//! There is no separate `team` field — the roster IS the operating
//! team, and "a team" is a global-only concept (a saved roster
//! template).
//!
//! User-scope keeps BOTH:
//! - `agents`: a by-name library of reusable agent DESCRIPTIONS
//!   (no role), so you can add an agent to a repo by name.
//! - `teams`: named [`TeamRoster`] templates. A team member
//!   ([`TeamMember`]) either REFERENCES a library agent by name
//!   (preferred — one source of truth) or carries a full INLINE
//!   definition for a team-only agent. `init --team` RESOLVES the
//!   references against the library into a repo's self-contained
//!   roster of full inline agents; `team save` publishes a repo's
//!   roster back up as references. References live only here — they
//!   never reach a repo config.
//!
//! All structs round-trip through serde with
//! `#[derive(Deserialize, Serialize)]`. No whole-document
//! `serde_json::Value` parsing — only `extra:
//! BTreeMap<String, Value>` flatten catchalls, which preserve
//! unknown keys for forward-compat.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use clank_core::agent_config::LaunchConfig;
use clank_core::ids::AgentLabel;
use clank_core::vocab::{AutoMode, Tool};

/// A roster: a flat list of agents-with-roles, keyed by label.
/// This is the shared shape — the repo's operating roster AND a
/// global team template are both `Roster`, so `init --team` /
/// `team save` are same-shape copies.
pub type Roster = BTreeMap<AgentLabel, RosterAgent>;

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
    /// The by-name DEFINITION library: reusable agent descriptions
    /// (NO role). `clank agent add <name>` (repo scope, no `--tool`)
    /// copies a description from here into a repo's roster.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<AgentLabel, AgentDescription>,
    /// Named TEAM templates. Each value is a [`TeamRoster`] — a map
    /// of [`TeamMember`]s that either REFERENCE a library agent
    /// (preferred) or carry an INLINE custom definition.
    /// `init --team <name>` resolves one into a repo's self-contained
    /// roster; `team save <name>` captures a repo's roster up here as
    /// references.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub teams: BTreeMap<String, TeamRoster>,
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
/// launch profile + optional initial prompt. NO role — the
/// by-name library (`UserConfigFile.agents`) stores these
/// role-free; the role is attached when an agent joins a roster
/// (see [`RosterAgent`]).
///
/// `PartialEq`/`Eq` back the `clank team save` collision check
/// (a referenced repo agent must match an identically-named
/// user-scope agent before it is published).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AgentDescription {
    pub tool: Tool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<String>,
}

/// An agent's role within a roster. Exclusive — one role per agent.
/// Reviewers differ by WHICH commits they're consulted on:
/// - `Commit`: every commit.
/// - `Plan`: only plan-doc commits (the plan milestone), after the
///   commit tier continues.
/// - `Final`: only the finish milestone, after the commit tier marks
///   FINISHED.
/// - `Gate`: both `Plan` and `Final` (the pre-split behavior; kept so
///   existing `role: gate` configs are unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RosterRole {
    Master,
    Commit,
    Plan,
    Final,
    Gate,
}

impl RosterRole {
    pub(crate) fn in_commit_tier(self) -> bool {
        self == Self::Commit
    }

    pub(crate) fn in_plan_tier(self) -> bool {
        matches!(self, Self::Plan | Self::Gate)
    }

    pub(crate) fn in_final_tier(self) -> bool {
        matches!(self, Self::Final | Self::Gate)
    }
}

/// One roster entry: an agent's DEFINITION plus its ROLE. This is
/// the value type of a [`Roster`]. The `role` field is what
/// distinguishes a roster entry from a bare [`AgentDescription`]
/// in the by-name library — an old-shape `{tool, ...}` repo
/// `agents` value (no `role`) fails to deserialize here, which is
/// how old configs fail-closed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RosterAgent {
    pub tool: Tool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<String>,
    pub role: RosterRole,
}

impl RosterAgent {
    /// Build a roster entry from a by-name [`AgentDescription`]
    /// plus a role (the copy-down path: `agent add <name>` /
    /// `init --team`).
    pub fn from_description(desc: AgentDescription, role: RosterRole) -> Self {
        Self {
            tool: desc.tool,
            launch: desc.launch,
            initial_prompt: desc.initial_prompt,
            role,
        }
    }

    /// Extract the role-free [`AgentDescription`] (the publish
    /// path: `team save` writes role-free descriptions into the
    /// user-scope `agents` library).
    pub fn to_description(&self) -> AgentDescription {
        AgentDescription {
            tool: self.tool,
            launch: self.launch.clone(),
            initial_prompt: self.initial_prompt.clone(),
        }
    }
}

/// A team template: members keyed by label. Each [`TeamMember`] is
/// either a REFERENCE into the by-name `agents` library (preferred —
/// one source of truth) or a full INLINE definition for a team-only
/// agent not in the library. Resolved into a [`Roster`] of full
/// inline agents when consumed by `init --team`; references never
/// reach a repo config.
pub type TeamRoster = BTreeMap<AgentLabel, TeamMember>;

/// One team-template member: a library reference or an inline custom
/// definition — the two kinds a team can be built from.
///
/// Serde is `untagged` with `Inline` FIRST: a [`RosterAgent`]
/// requires `tool`, so a reference value (which has no `tool`) can
/// never match `Inline` and falls through to `Ref`. This is what
/// lets old all-inline team configs keep deserializing unchanged
/// while new ref-shaped entries (`{"role":"gate"}`) parse as `Ref`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum TeamMember {
    /// A full inline definition for a team-only agent not in the
    /// library (same shape as a repo roster entry).
    Inline(RosterAgent),
    /// A reference to a library agent, carrying only the role.
    Ref(TeamRef),
}

/// A reference from a team to a by-name `agents` library entry. Carries
/// only the role; `tool`/`launch`/`initial_prompt` resolve from the
/// library when the team is consumed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TeamRef {
    /// Library agent to resolve against. Defaults to the team entry's
    /// KEY; set it only to alias a differently-named library agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentLabel>,
    pub role: RosterRole,
}

impl TeamMember {
    /// The role this member plays, regardless of kind.
    pub fn role(&self) -> RosterRole {
        match self {
            TeamMember::Inline(agent) => agent.role,
            TeamMember::Ref(r) => r.role,
        }
    }

    /// Resolve to a concrete [`RosterAgent`]. `key` is the team
    /// entry's label — the default library name for a [`TeamRef`]
    /// with no explicit `agent`.
    pub fn resolve(
        &self,
        key: &AgentLabel,
        library: &BTreeMap<AgentLabel, AgentDescription>,
    ) -> Result<RosterAgent, TeamResolveError> {
        match self {
            TeamMember::Inline(agent) => Ok(agent.clone()),
            TeamMember::Ref(r) => {
                let target = r.agent.as_ref().unwrap_or(key);
                let desc = library
                    .get(target)
                    .ok_or_else(|| TeamResolveError::UnknownRef {
                        member: key.clone(),
                        target: target.clone(),
                    })?;
                Ok(RosterAgent::from_description(desc.clone(), r.role))
            }
        }
    }
}

/// Resolve a whole team template into a concrete [`Roster`] of full
/// inline agents, resolving every [`TeamMember::Ref`] against the
/// `agents` library. The home-config → repo-config bridge:
/// `init --team` calls it so a repo receives a self-contained roster
/// (references never reach a repo config).
pub fn resolve_team(
    team: &TeamRoster,
    library: &BTreeMap<AgentLabel, AgentDescription>,
) -> Result<Roster, TeamResolveError> {
    team.iter()
        .map(|(label, member)| Ok((label.clone(), member.resolve(label, library)?)))
        .collect()
}

/// A [`TeamMember::Ref`] pointed at a library agent that doesn't
/// exist. Raised when consuming a team (`init --team`).
#[derive(Debug, thiserror::Error)]
pub enum TeamResolveError {
    #[error(
        "team member `{member}` references library agent `{target}`, which is not in \
         user-scope `agents`; add it (`clank agent add {target} --global --tool <tool>`) \
         or define the member inline"
    )]
    UnknownRef {
        member: AgentLabel,
        target: AgentLabel,
    },
}

/// What kind of review a reviewer does — names what they review, not
/// an abstract "tier". Maps 1:1 onto the reviewer [`RosterRole`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewKind {
    Commit,
    Plan,
    Final,
    Gate,
}

impl From<ReviewKind> for RosterRole {
    fn from(k: ReviewKind) -> Self {
        match k {
            ReviewKind::Commit => RosterRole::Commit,
            ReviewKind::Plan => RosterRole::Plan,
            ReviewKind::Final => RosterRole::Final,
            ReviewKind::Gate => RosterRole::Gate,
        }
    }
}

/// Repo-scope `<repo>/.clank/config.json`. Gitignored by
/// `clank init` — config is per-user-per-repo. The repo's
/// `agents` IS the operating roster: a flat
/// `BTreeMap<AgentLabel, RosterAgent>` where each entry carries
/// its definition AND its role. There is no separate `team`
/// field — the roster is self-contained.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RepoConfigFile {
    /// The operating roster: agents-with-roles. Exactly one entry
    /// should have `role == Master` (validated at resolve, not
    /// deserialize).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: Roster,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<crate::cli::config::ReviewSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<crate::cli::config::HooksSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<crate::cli::config::DiffConfig>,
    /// Forward-compat catchall for unknown OBJECT fields. Old-shape
    /// markers do NOT reach here: the loader's legacy-shape check
    /// fail-closes a `team`/`promoted` key (or an array `agents`)
    /// before the typed parse is accepted.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Resolved registered set for a repo: the master plus ONE role-tagged
/// reviewer collection. Reviewers are a single `Vec`, NOT per-role
/// buckets — so "enumerate every reviewer" is just `reviewers` (nothing
/// to chain, no role can be silently dropped), and the role→tier
/// mapping (incl. the `gate = plan + final` fold) lives in exactly one
/// place: the tier accessors below.
#[derive(Debug, Clone)]
pub struct RegisteredSet {
    pub master: AgentLabel,
    pub master_desc: AgentDescription,
    /// Every reviewer with its role. THE source of truth for reviewer
    /// enumeration — display surfaces iterate this and read `role`.
    pub reviewers: Vec<RegisteredReviewer>,
}

impl RegisteredSet {
    fn labels_with_role(&self, want: impl Fn(RosterRole) -> bool) -> Vec<AgentLabel> {
        self.reviewers
            .iter()
            .filter(|r| want(r.role))
            .map(|r| r.label.clone())
            .collect()
    }

    /// Commit-tier reviewers (every commit): role `Commit`.
    pub fn commit_tier(&self) -> Vec<AgentLabel> {
        self.labels_with_role(RosterRole::in_commit_tier)
    }

    /// PLAN-milestone tier (plan-doc commits): `plan` + `gate`.
    pub fn plan_tier(&self) -> Vec<AgentLabel> {
        self.labels_with_role(RosterRole::in_plan_tier)
    }

    /// FINISH-milestone tier (the end): `final` + `gate`.
    pub fn final_tier(&self) -> Vec<AgentLabel> {
        self.labels_with_role(RosterRole::in_final_tier)
    }
}

/// One resolved reviewer: its label, description, and roster role
/// (`Commit` / `Plan` / `Final` / `Gate` — never `Master`).
#[derive(Debug, Clone)]
pub struct RegisteredReviewer {
    pub label: AgentLabel,
    pub desc: AgentDescription,
    pub role: RosterRole,
}

/// Errors raised while resolving the registered set for a repo.
/// Roles are exclusive in a roster, so "master also reviewer" /
/// "reviewer in both lists" / "duplicate reviewer" are now
/// structurally impossible — those variants are gone.
#[derive(Debug, thiserror::Error)]
pub enum ResolutionError {
    #[error("no master set; run `clank agent promote <agent>`")]
    NoMaster,
    #[error(
        "multiple masters in the roster ({0}); exactly one agent may have role `master` — \
         run `clank agent promote <agent>` to pick one"
    )]
    MultipleMasters(String),
}

/// Resolve the registered set from a repo's roster. The master is
/// the single entry with `role == Master` (zero →
/// [`ResolutionError::NoMaster`], more than one →
/// [`ResolutionError::MultipleMasters`]); commit/gate reviewers
/// are the entries with the respective role. The repo is
/// self-contained — every agent's definition lives in the roster.
pub fn resolve_registered_set(repo: &RepoConfigFile) -> Result<RegisteredSet, ResolutionError> {
    let mut master: Option<(AgentLabel, AgentDescription)> = None;
    let mut reviewers = Vec::new();

    for (label, agent) in &repo.agents {
        match agent.role {
            RosterRole::Master => {
                if let Some((existing, _)) = &master {
                    return Err(ResolutionError::MultipleMasters(format!(
                        "{}, {}",
                        existing.as_str(),
                        label.as_str()
                    )));
                }
                master = Some((label.clone(), agent.to_description()));
            }
            // Every non-master role is a reviewer in the single
            // collection, tagged with its role.
            role => reviewers.push(RegisteredReviewer {
                label: label.clone(),
                desc: agent.to_description(),
                role,
            }),
        }
    }

    let (master, master_desc) = master.ok_or(ResolutionError::NoMaster)?;
    Ok(RegisteredSet {
        master,
        master_desc,
        reviewers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
    }

    fn roster_agent(tool: Tool, role: RosterRole) -> RosterAgent {
        RosterAgent {
            tool,
            launch: None,
            initial_prompt: None,
            role,
        }
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
        let mut dev: TeamRoster = BTreeMap::new();
        dev.insert(label("claude"), team_ref(RosterRole::Master));
        dev.insert(label("codex"), team_ref(RosterRole::Commit));
        dev.insert(label("ruthless"), team_ref(RosterRole::Gate));
        teams.insert("dev".to_string(), dev);
        let cfg = UserConfigFile {
            agents,
            teams,
            ..Default::default()
        };
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let back: UserConfigFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agents.len(), 2);
        assert_eq!(back.teams.len(), 1);
        let dev = back.teams.get("dev").unwrap();
        assert_eq!(
            dev.get(&label("claude")).unwrap().role(),
            RosterRole::Master
        );
        assert_eq!(
            dev.get(&label("ruthless")).unwrap().role(),
            RosterRole::Gate
        );
    }

    fn team_ref(role: RosterRole) -> TeamMember {
        TeamMember::Ref(TeamRef { agent: None, role })
    }

    #[test]
    fn team_member_ref_round_trips_as_role_only_object() {
        let m = team_ref(RosterRole::Gate);
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, r#"{"role":"gate"}"#);
        assert_eq!(serde_json::from_str::<TeamMember>(&json).unwrap(), m);
    }

    #[test]
    fn team_member_ref_with_alias_round_trips() {
        let m = TeamMember::Ref(TeamRef {
            agent: Some(label("ruthless")),
            role: RosterRole::Gate,
        });
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, r#"{"agent":"ruthless","role":"gate"}"#);
        assert_eq!(serde_json::from_str::<TeamMember>(&json).unwrap(), m);
    }

    #[test]
    fn team_member_inline_value_parses_as_inline_not_ref() {
        // A value WITH `tool` is an inline custom definition — it must
        // not be mistaken for a ref (this is the back-compat path: old
        // all-inline team configs keep loading).
        let m: TeamMember = serde_json::from_str(r#"{"tool":"claude","role":"commit"}"#).unwrap();
        match m {
            TeamMember::Inline(a) => {
                assert_eq!(a.tool, Tool::Claude);
                assert_eq!(a.role, RosterRole::Commit);
            }
            TeamMember::Ref(_) => panic!("a value with `tool` must parse as Inline"),
        }
    }

    #[test]
    fn team_resolve_ref_pulls_definition_from_library() {
        let mut library = BTreeMap::new();
        library.insert(
            label("ruthless"),
            AgentDescription {
                tool: Tool::Claude,
                launch: Some(LaunchConfig {
                    command: None,
                    args: vec!["--agent".into(), "ruthless-code-reviewer".into()],
                    env: Default::default(),
                }),
                initial_prompt: None,
            },
        );
        let mut team: TeamRoster = BTreeMap::new();
        // Ref by key (default name) and an aliased ref.
        team.insert(label("ruthless"), team_ref(RosterRole::Gate));
        team.insert(
            label("second"),
            TeamMember::Ref(TeamRef {
                agent: Some(label("ruthless")),
                role: RosterRole::Commit,
            }),
        );
        // Plus an inline custom member with no library entry.
        team.insert(
            label("oneoff"),
            TeamMember::Inline(roster_agent(Tool::Codex, RosterRole::Master)),
        );

        let roster = resolve_team(&team, &library).unwrap();
        let gate = roster.get(&label("ruthless")).unwrap();
        assert_eq!(gate.role, RosterRole::Gate);
        assert_eq!(
            gate.launch.as_ref().unwrap().args,
            vec!["--agent", "ruthless-code-reviewer"]
        );
        // Aliased ref resolves the same definition with its own role.
        let second = roster.get(&label("second")).unwrap();
        assert_eq!(second.role, RosterRole::Commit);
        assert_eq!(second.tool, Tool::Claude);
        // Inline member is copied verbatim.
        assert_eq!(roster.get(&label("oneoff")).unwrap().tool, Tool::Codex);
    }

    #[test]
    fn team_resolve_dangling_ref_errors() {
        let library = BTreeMap::new();
        let mut team: TeamRoster = BTreeMap::new();
        team.insert(label("ghost"), team_ref(RosterRole::Gate));
        let err = resolve_team(&team, &library).unwrap_err();
        match err {
            TeamResolveError::UnknownRef { member, target } => {
                assert_eq!(member.as_str(), "ghost");
                assert_eq!(target.as_str(), "ghost");
            }
        }
    }

    #[test]
    fn roster_role_serializes_snake_case() {
        let a = roster_agent(Tool::Claude, RosterRole::Master);
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains("\"role\":\"master\""), "got: {json}");
        let g = roster_agent(Tool::Codex, RosterRole::Gate);
        let json = serde_json::to_string(&g).unwrap();
        assert!(json.contains("\"role\":\"gate\""), "got: {json}");
    }

    #[test]
    fn roster_agent_without_role_fails_to_deserialize() {
        // The role field is required: an old-shape `AgentDescription`
        // value (no role) fails to parse as a RosterAgent. This is
        // how an old `{agents: <descriptions>}` repo config
        // fail-closes.
        let err = serde_json::from_str::<RosterAgent>(r#"{"tool":"claude"}"#);
        assert!(err.is_err(), "RosterAgent must require `role`");
    }

    #[test]
    fn repo_config_empty_round_trips() {
        let cfg = RepoConfigFile::default();
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(json, "{}");
        let back: RepoConfigFile = serde_json::from_str(&json).unwrap();
        assert!(back.agents.is_empty());
    }

    #[test]
    fn repo_config_full_round_trips() {
        let cfg = repo_with(vec![
            ("claude", Tool::Claude, RosterRole::Master),
            ("codex", Tool::Codex, RosterRole::Commit),
            ("ruthless", Tool::Claude, RosterRole::Gate),
        ]);
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let back: RepoConfigFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agents.len(), 3);
        assert_eq!(
            back.agents.get(&label("claude")).unwrap().role,
            RosterRole::Master
        );
        assert_eq!(
            back.agents.get(&label("codex")).unwrap().role,
            RosterRole::Commit
        );
    }

    // ── resolve_registered_set ───────────────────────────

    fn repo_with(agents: Vec<(&str, Tool, RosterRole)>) -> RepoConfigFile {
        let mut roster: Roster = BTreeMap::new();
        for (l, t, role) in agents {
            roster.insert(label(l), roster_agent(t, role));
        }
        RepoConfigFile {
            agents: roster,
            ..Default::default()
        }
    }

    #[test]
    fn resolve_master_and_commit_reviewer() {
        let repo = repo_with(vec![
            ("claude", Tool::Claude, RosterRole::Master),
            ("codex", Tool::Codex, RosterRole::Commit),
        ]);
        let r = resolve_registered_set(&repo).unwrap();
        assert_eq!(r.master.as_str(), "claude");
        assert_eq!(r.master_desc.tool, Tool::Claude);
        assert_eq!(r.reviewers.len(), 1);
        assert_eq!(r.reviewers[0].label.as_str(), "codex");
        assert_eq!(r.reviewers[0].role, RosterRole::Commit);
        assert_eq!(r.reviewers[0].desc.tool, Tool::Codex);
        assert!(r.plan_tier().is_empty() && r.final_tier().is_empty());
    }

    #[test]
    fn resolve_with_gate_reviewers() {
        let repo = repo_with(vec![
            ("claude", Tool::Claude, RosterRole::Master),
            ("codex", Tool::Codex, RosterRole::Commit),
            ("ruthless", Tool::Claude, RosterRole::Gate),
        ]);
        let r = resolve_registered_set(&repo).unwrap();
        assert_eq!(r.master.as_str(), "claude");
        assert_eq!(r.commit_tier(), vec![label("codex")]);
        // gate folds into BOTH compute tiers.
        assert_eq!(r.plan_tier(), vec![label("ruthless")]);
        assert_eq!(r.final_tier(), vec![label("ruthless")]);
    }

    #[test]
    fn resolve_buckets_plan_final_gate_and_folds_gate_into_both_tiers() {
        let repo = repo_with(vec![
            ("claude", Tool::Claude, RosterRole::Master),
            ("codex", Tool::Codex, RosterRole::Commit),
            ("scout", Tool::Codex, RosterRole::Plan),
            ("shipper", Tool::Claude, RosterRole::Final),
            ("ruthless", Tool::Claude, RosterRole::Gate),
        ]);
        let r = resolve_registered_set(&repo).unwrap();
        // One role-tagged collection — a gate reviewer is listed ONCE.
        assert_eq!(r.reviewers.len(), 4);
        let role_of = |name: &str| {
            r.reviewers
                .iter()
                .find(|x| x.label.as_str() == name)
                .map(|x| x.role)
        };
        assert_eq!(role_of("codex"), Some(RosterRole::Commit));
        assert_eq!(role_of("scout"), Some(RosterRole::Plan));
        assert_eq!(role_of("shipper"), Some(RosterRole::Final));
        assert_eq!(role_of("ruthless"), Some(RosterRole::Gate));
        // Compute tiers fold gate into BOTH plan and final.
        let plan_tier: Vec<String> = r
            .plan_tier()
            .iter()
            .map(|l| l.as_str().to_string())
            .collect();
        let final_tier: Vec<String> = r
            .final_tier()
            .iter()
            .map(|l| l.as_str().to_string())
            .collect();
        assert!(
            plan_tier.contains(&"scout".to_string()) && plan_tier.contains(&"ruthless".to_string())
        );
        assert!(
            !plan_tier.contains(&"shipper".to_string()),
            "plan tier excludes final-only"
        );
        assert!(
            final_tier.contains(&"shipper".to_string())
                && final_tier.contains(&"ruthless".to_string())
        );
        assert!(
            !final_tier.contains(&"scout".to_string()),
            "final tier excludes plan-only"
        );
    }

    #[test]
    fn resolve_empty_bootstrapped_repo_errors_no_master() {
        let repo = RepoConfigFile::default();
        let err = resolve_registered_set(&repo).unwrap_err();
        assert!(matches!(err, ResolutionError::NoMaster));
        let msg = err.to_string();
        assert!(msg.contains("clank agent promote"));
    }

    #[test]
    fn resolve_zero_masters_errors_no_master() {
        // A roster with only reviewers (no master) → NoMaster.
        let repo = repo_with(vec![
            ("codex", Tool::Codex, RosterRole::Commit),
            ("ruthless", Tool::Claude, RosterRole::Gate),
        ]);
        let err = resolve_registered_set(&repo).unwrap_err();
        assert!(matches!(err, ResolutionError::NoMaster));
    }

    #[test]
    fn resolve_two_masters_errors_multiple_masters() {
        let repo = repo_with(vec![
            ("claude", Tool::Claude, RosterRole::Master),
            ("codex", Tool::Codex, RosterRole::Master),
        ]);
        let err = resolve_registered_set(&repo).unwrap_err();
        match err {
            ResolutionError::MultipleMasters(s) => {
                assert!(s.contains("claude") && s.contains("codex"), "got: {s}");
            }
            other => panic!("expected MultipleMasters; got {other:?}"),
        }
    }
}
