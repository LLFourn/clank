//! File-IO for per-agent (`.clank/agents/<label>/config.json`)
//! and repo-level (`.clank/config.json`) typed configs.
//!
//! Pure types live in `clank_core::agent_config`; this module is
//! the only place that knows where they sit on disk and how to
//! load/save them atomically.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use clank_core::agent_config::{AgentConfig, Session};
use clank_core::ids::{AgentLabel, SessionId};
use clank_core::vocab::Tool;

/// Repo-relative path: `.clank/agents`.
pub fn agents_root(repo: &Path) -> PathBuf {
    repo.join(".clank").join("agents")
}

/// Repo-relative path: `.clank/agents/<label>/config.json`.
pub fn agent_config_path(repo: &Path, label: &AgentLabel) -> PathBuf {
    agents_root(repo).join(label.as_str()).join("config.json")
}

/// Load this agent's config from disk. Returns `Ok(None)` if the
/// file doesn't exist (caller may default-construct); other I/O
/// or JSON errors propagate.
pub fn load_agent_config(repo: &Path, label: &AgentLabel) -> anyhow::Result<Option<AgentConfig>> {
    let path = agent_config_path(repo, label);
    load_json(&path).with_context(|| format!("reading `{}`", path.display()))
}

/// Write this agent's config to disk atomically. Creates parent
/// dirs if needed.
pub fn save_agent_config(repo: &Path, label: &AgentLabel, cfg: &AgentConfig) -> anyhow::Result<()> {
    let path = agent_config_path(repo, label);
    save_json(&path, cfg).with_context(|| format!("writing `{}`", path.display()))
}

/// List every agent in this repo's `.clank/agents/`, propagating
/// parse / I/O errors. Use this when a partial result would be
/// dangerous — `clank as` (must see ALL existing bindings to
/// safely clear stale ones), `clank auto`, `clank doctor`.
pub fn load_all_agent_configs(repo: &Path) -> anyhow::Result<Vec<(AgentLabel, AgentConfig)>> {
    let mut out = Vec::new();
    for entry in iter_agent_dirs(repo)? {
        let (label, _) = entry?;
        if let Some(cfg) = load_agent_config(repo, &label)? {
            out.push((label, cfg));
        }
    }
    Ok(out)
}

/// Error returned by the team-based resolvers when a repo has no
/// `team` field configured. No legacy fallback exists: a repo
/// without a team is a setup error, not a master-only default.
fn no_team_configured() -> anyhow::Error {
    anyhow::anyhow!("this repo has no team configured. Run `clank init --team <name>` to set one.")
}

/// Resolve an agent's role for this repo via the team-based
/// registered set: `label == master` → `Master`; in either
/// reviewer tier → `Reviewer`; otherwise the default role.
///
/// Errors if the repo has no `team` field set — there is no
/// legacy fallback.
pub fn resolve_role(repo: &Path, label: &AgentLabel) -> anyhow::Result<clank_core::vocab::Role> {
    match try_resolve_via_team(repo)? {
        Some(set) => Ok(role_from_registered_set(&set, label)),
        None => Err(no_team_configured()),
    }
}

/// Plan: teams-based-agent-registration.
///
/// Two-tier reviewer split for a repo. Used by `WorkPolicy`
/// construction sites (status, wfw, open) to feed the
/// two-tier gate state machine.
///
/// Returns `(commit_reviewers, gate_reviewers)` from the
/// resolved registered set. Errors if the repo has no `team`
/// field set — there is no legacy fallback.
pub fn load_reviewer_tiers(repo: &Path) -> anyhow::Result<(Vec<AgentLabel>, Vec<AgentLabel>)> {
    match try_resolve_via_team(repo)? {
        Some(set) => {
            let commit = set.commit_reviewers.into_iter().map(|a| a.label).collect();
            let gate = set.gate_reviewers.into_iter().map(|a| a.label).collect();
            Ok((commit, gate))
        }
        None => Err(no_team_configured()),
    }
}

/// Reviewer tiers for READ-ONLY render paths (`clank status`,
/// `clank html`). Unlike [`load_reviewer_tiers`], this never
/// errors: a repo with no team (or a misconfigured one) degrades
/// to empty tiers so a read-only renderer never crashes on
/// config absence. The gate then computes as zero-reviewer
/// (Approved). `clank doctor` is the surface that reports the
/// underlying misconfiguration. Plan:
/// `teams-based-agent-registration`.
pub fn reviewer_tiers_for_render(repo: &Path) -> (Vec<AgentLabel>, Vec<AgentLabel>) {
    match try_resolve_via_team(repo) {
        Ok(Some(set)) => (
            set.commit_reviewers.into_iter().map(|a| a.label).collect(),
            set.gate_reviewers.into_iter().map(|a| a.label).collect(),
        ),
        _ => (Vec::new(), Vec::new()),
    }
}

/// Plan: teams-based-agent-registration (phase 6b).
///
/// Shared dispatch helper for `load_reviewer_tiers`,
/// `reviewer_tiers_for_render`, and `resolve_role`. Returns:
/// - `Ok(Some(RegisteredSet))` when the repo config parses
///   AND has a `team` field. The set is the result of running
///   `resolve_registered_set` against the new typed schemas.
/// - `Ok(None)` when the repo has no `team` field set (or no
///   repo config exists). This is "no team configured" — the
///   callers handle it per the render-vs-workflow boundary:
///   workflow commands error (`no_team_configured()`),
///   read-only renderers degrade to empty tiers. There is no
///   legacy fallback.
///
/// Failure modes:
/// - Repo config exists but new-schema parse fails (a
///   strongly-typed field like `team` has the wrong shape, e.g.
///   `team: 42`): returns an error (fail-closed). A legacy
///   `agents` array does NOT trigger this — the new
///   `RepoConfigFile` has no `agents` field, so the array lands
///   in the `extra` flatten catchall, parse SUCCEEDS, and the
///   repo falls through to `Ok(None)` (no team).
/// - User config doesn't parse as new-schema when team is
///   set: error propagates (fail-closed).
/// - `resolve_registered_set` returns error (UnknownTeam,
///   NoMaster, etc.): error propagates.
/// Plan: teams-based-agent-registration (codex b59dafb pure
/// helper). Derive a role from a resolved registered set.
/// Pure: no $HOME, no filesystem. Testable directly.
pub fn role_from_registered_set(
    set: &crate::cli::teams_config::RegisteredSet,
    label: &AgentLabel,
) -> clank_core::vocab::Role {
    use clank_core::vocab::Role;
    if &set.master == label {
        return Role::Master;
    }
    let in_either_tier = set
        .commit_reviewers
        .iter()
        .chain(set.gate_reviewers.iter())
        .any(|a| &a.label == label);
    if in_either_tier {
        Role::Reviewer
    } else {
        Role::default()
    }
}

pub fn try_resolve_via_team(
    repo: &Path,
) -> anyhow::Result<Option<crate::cli::teams_config::RegisteredSet>> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    try_resolve_via_team_with(repo, home.as_deref())
}

/// Plan: teams-based-agent-registration (ruthless pin 1).
///
/// Testable workhorse that the production [`try_resolve_via_team`]
/// wraps with `$HOME`. Same semantics; explicit `home`
/// parameter so tests can seed both repo and user configs
/// without env mutation.
fn try_resolve_via_team_with(
    repo: &Path,
    home: Option<&Path>,
) -> anyhow::Result<Option<crate::cli::teams_config::RegisteredSet>> {
    use crate::cli::teams_config::{RepoConfigFile, UserConfigFile, resolve_registered_set};

    let repo_cfg_path = repo.join(".clank/config.json");
    let body = match std::fs::read_to_string(&repo_cfg_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    // Fail-closed on parse error (codex b59dafb catch). Legacy
    // `agents` array shape DOES parse here — the new schema's
    // `agents` field is a BTreeMap, not a Vec, so legacy gets
    // captured in `extra`. A real parse error means the file
    // is genuinely malformed (e.g. invalid JSON, unknown
    // strongly-typed field shape) and should surface, not be
    // silently swallowed by the legacy fallback.
    let repo_cfg: RepoConfigFile = serde_json::from_str(&body).map_err(|e| {
        anyhow::anyhow!(
            "parsing {} as new-schema RepoConfigFile: {e}",
            repo_cfg_path.display()
        )
    })?;
    if repo_cfg.team.is_none() {
        return Ok(None);
    }

    // Team is set: read user-scope new schema and resolve.
    let user_cfg: UserConfigFile = match home {
        Some(h) => {
            let p = h.join(".clank/config.json");
            match std::fs::read_to_string(&p) {
                Ok(s) => serde_json::from_str(&s).map_err(|e| {
                    anyhow::anyhow!("parsing {} as new-schema UserConfigFile: {e}", p.display())
                })?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => UserConfigFile::default(),
                Err(e) => return Err(e.into()),
            }
        }
        None => UserConfigFile::default(),
    };

    Ok(Some(resolve_registered_set(&user_cfg, &repo_cfg)?))
}

/// Same as [`load_all_agent_configs`] but silently drops agents
/// whose `config.json` failed to parse. Use this for the identity
/// resolver path where a single broken config shouldn't take
/// down every other lookup. `clank doctor` is the place that
/// surfaces the broken file.
pub fn load_all_agent_configs_lossy(repo: &Path) -> anyhow::Result<Vec<(AgentLabel, AgentConfig)>> {
    let mut out = Vec::new();
    for entry in iter_agent_dirs(repo)? {
        let Ok((label, _)) = entry else {
            continue;
        };
        match load_agent_config(repo, &label) {
            Ok(Some(cfg)) => out.push((label, cfg)),
            Ok(None) | Err(_) => continue,
        }
    }
    Ok(out)
}

/// Iterate `.clank/agents/*` returning each (label, dir-path)
/// that parses. The outer Result is for the read_dir on the
/// `agents/` root; per-entry errors are propagated as inner
/// `Result`s so callers can decide whether to drop or surface.
fn iter_agent_dirs(
    repo: &Path,
) -> anyhow::Result<impl Iterator<Item = anyhow::Result<(AgentLabel, PathBuf)>>> {
    let root = agents_root(repo);
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::<anyhow::Result<(AgentLabel, PathBuf)>>::new().into_iter());
        }
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("reading `{}`", root.display())));
        }
    };
    let mut out: Vec<anyhow::Result<(AgentLabel, PathBuf)>> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                out.push(Err(e.into()));
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(e) => {
                out.push(Err(e.into()));
                continue;
            }
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        match AgentLabel::parse(&name) {
            Ok(label) => out.push(Ok((label, entry.path()))),
            Err(_) => continue,
        }
    }
    Ok(out.into_iter())
}

/// Outcome of [`bind_session_to_agent`]: the bound agent's
/// label (echoed for diagnostics) plus the labels of any OTHER
/// agents whose stale binding to the same session id was cleared
/// in the process. Both `clank as` and `clank init` phase 2
/// surface the cleared list to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindOutcome {
    pub label: AgentLabel,
    pub cleared_from: Vec<AgentLabel>,
}

/// Bind a session to an agent label, atomically updating the
/// target agent's `config.json` AND clearing the same session id
/// from any OTHER agent's config that currently holds it.
///
/// Single source of truth for the bind operation — used by
/// `clank as` AND `clank init` phase 2 so the two-shells-one-
/// session uniqueness invariant can't be re-introduced by a
/// caller that forgets the clear-stale step.
///
/// Ordering: load all configs STRICT (parse errors propagate),
/// identify stale, write the target FIRST (so a partial failure
/// on stale-clear leaves a ghost binding rather than no
/// binding), then clear stale. Preserves auto_mode and
/// wfw_timeout on the target's existing config.
pub fn bind_session_to_agent(
    repo: &Path,
    label: &AgentLabel,
    tool: Tool,
    session_id: &SessionId,
) -> anyhow::Result<BindOutcome> {
    let all = load_all_agent_configs(repo)?;
    let stale: Vec<(AgentLabel, AgentConfig)> = all
        .into_iter()
        .filter(|(other_label, cfg)| {
            other_label != label && cfg.session.as_ref().is_some_and(|s| &s.id == session_id)
        })
        .collect();

    // Bind the new label FIRST. Order matters: if the clear-stale
    // step below partially fails, the worst case is a ghost
    // binding on the old agent (which `clank as` or `clank doctor`
    // can clean up later). The reverse (clear-then-bind) could
    // leave them with neither.
    let mut cfg = load_agent_config(repo, label)?.unwrap_or_default();
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("formatting timestamp")?;
    cfg.session = Some(Session {
        id: session_id.clone(),
        tool,
        updated_at: now,
    });
    save_agent_config(repo, label, &cfg)?;

    let mut cleared_from: Vec<AgentLabel> = Vec::new();
    for (other_label, mut other_cfg) in stale {
        other_cfg.session = None;
        save_agent_config(repo, &other_label, &other_cfg)
            .with_context(|| format!("clearing stale binding on `{}`", other_label.as_str()))?;
        cleared_from.push(other_label);
    }

    Ok(BindOutcome {
        label: label.clone(),
        cleared_from,
    })
}

fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let parsed = serde_json::from_str(&s)
                .with_context(|| format!("parsing `{}` as JSON", path.display()))?;
            Ok(Some(parsed))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn save_json<T: serde::Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for `{}`", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".clank-config-")
        .suffix(".json.tmp")
        .tempfile_in(parent)?;
    let pretty = serde_json::to_string_pretty(value)?;
    tmp.write_all(pretty.as_bytes())?;
    // Trailing newline — every editor adds one; matching it
    // avoids a diff every time a human opens the file.
    tmp.write_all(b"\n")?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::teams_config::{AgentDescription, RegisteredSet, ResolvedAgent};
    use clank_core::vocab::{Role, Tool};

    fn label(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
    }

    fn desc(tool: Tool) -> AgentDescription {
        AgentDescription {
            tool,
            launch: None,
            initial_prompt: None,
        }
    }

    fn registered_set(master: &str, commit: &[&str], gate: &[&str]) -> RegisteredSet {
        RegisteredSet {
            master: label(master),
            master_desc: desc(Tool::Claude),
            commit_reviewers: commit
                .iter()
                .map(|l| ResolvedAgent {
                    label: label(l),
                    desc: desc(Tool::Claude),
                })
                .collect(),
            gate_reviewers: gate
                .iter()
                .map(|l| ResolvedAgent {
                    label: label(l),
                    desc: desc(Tool::Claude),
                })
                .collect(),
        }
    }

    #[test]
    fn role_from_registered_set_returns_master_for_team_master() {
        // Codex b59dafb catch: the team master must be
        // Role::Master regardless of what the legacy
        // `default_agents` entry says. Pre-fix, the role
        // resolver was reading the legacy merged declaration
        // and returning Role::Reviewer for a team master whose
        // legacy entry said reviewer — leading to master work
        // items never emitting via wfw.
        let set = registered_set("codex", &["claude"], &["ruthless"]);
        assert_eq!(
            role_from_registered_set(&set, &label("codex")),
            Role::Master
        );
    }

    #[test]
    fn role_from_registered_set_returns_reviewer_for_commit_tier() {
        let set = registered_set("codex", &["claude"], &["ruthless"]);
        assert_eq!(
            role_from_registered_set(&set, &label("claude")),
            Role::Reviewer
        );
    }

    #[test]
    fn role_from_registered_set_returns_reviewer_for_gate_tier() {
        let set = registered_set("codex", &["claude"], &["ruthless"]);
        assert_eq!(
            role_from_registered_set(&set, &label("ruthless")),
            Role::Reviewer
        );
    }

    #[test]
    fn role_from_registered_set_returns_default_for_unknown_label() {
        let set = registered_set("codex", &["claude"], &["ruthless"]);
        assert_eq!(
            role_from_registered_set(&set, &label("phantom")),
            Role::default()
        );
    }

    // ── try_resolve_via_team_with — dispatch coverage
    //    (ruthless pin 1 on 8c0fdb2: the dispatch helper had
    //    no tests despite codex catching two bugs in it). ──

    use crate::cli::teams_config::{TeamComposition, UserConfigFile};
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn write_repo_config(repo: &Path, body: &str) {
        let p = repo.join(".clank/config.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
    }

    fn write_user_config_with_team(home: &Path) {
        let mut user_cfg = UserConfigFile::default();
        user_cfg.agents.insert(
            label("codex"),
            AgentDescription {
                tool: Tool::Codex,
                launch: None,
                initial_prompt: None,
            },
        );
        user_cfg.agents.insert(
            label("claude"),
            AgentDescription {
                tool: Tool::Claude,
                launch: None,
                initial_prompt: None,
            },
        );
        let mut teams = BTreeMap::new();
        teams.insert(
            "dev".to_string(),
            TeamComposition {
                master: Some(label("codex")),
                commit_reviewers: vec![label("claude")],
                gate_reviewers: vec![],
            },
        );
        user_cfg.teams = teams;
        let p = home.join(".clank/config.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, serde_json::to_string_pretty(&user_cfg).unwrap()).unwrap();
    }

    #[test]
    fn try_resolve_via_team_with_returns_none_when_repo_config_missing() {
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let r = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap();
        assert!(r.is_none(), "no repo config → None (legacy fallback)");
    }

    #[test]
    fn try_resolve_via_team_with_returns_none_when_no_team_field() {
        // Repo config exists but has no `team` field set.
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(repo.path(), "{}");
        let r = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap();
        assert!(r.is_none(), "no team field → None (legacy fallback)");
    }

    #[test]
    fn try_resolve_via_team_with_returns_set_when_team_resolves() {
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_user_config_with_team(home.path());
        write_repo_config(repo.path(), r#"{"team": "dev"}"#);
        let set = try_resolve_via_team_with(repo.path(), Some(home.path()))
            .unwrap()
            .expect("team field present → Some(set)");
        assert_eq!(set.master.as_str(), "codex");
        assert_eq!(set.commit_reviewers.len(), 1);
        assert_eq!(set.commit_reviewers[0].label.as_str(), "claude");
    }

    #[test]
    fn try_resolve_via_team_with_fails_closed_on_malformed_repo_config() {
        // Codex b59dafb fail-closed catch: a present-but-
        // malformed `team` field must error, NOT fall back to
        // legacy reviewers. Here `team: 42` is structurally
        // invalid for the TeamField untagged enum.
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(repo.path(), r#"{"team": 42}"#);
        let err = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("RepoConfigFile"),
            "expected parse-failure error for malformed team field; got: {msg}"
        );
    }

    #[test]
    fn try_resolve_via_team_with_legacy_agents_array_lands_in_extra_returns_none() {
        // Back-compat property: a legacy `agents` array shape
        // parses cleanly via the new schema (lands in `extra`),
        // and the absence of a `team` field then routes to
        // legacy fallback. A regression here would fail-closed
        // on every unmigrated repo — disastrous.
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(
            repo.path(),
            r#"{"agents": [{"label": "claude", "role": "master", "tool": "claude"}]}"#,
        );
        let r = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap();
        assert!(
            r.is_none(),
            "legacy agents shape parses + no team field → None (legacy fallback)"
        );
    }

    #[test]
    fn try_resolve_via_team_with_propagates_resolver_errors() {
        // Team is set but the named team doesn't exist in
        // user-scope → ResolutionError::UnknownTeam propagates.
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        // user-scope has no teams.
        std::fs::create_dir_all(home.path().join(".clank")).unwrap();
        std::fs::write(home.path().join(".clank/config.json"), "{}").unwrap();
        write_repo_config(repo.path(), r#"{"team": "phantom"}"#);
        let err = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("phantom") || msg.contains("not declared") || msg.contains("UnknownTeam"),
            "expected UnknownTeam error; got: {msg}"
        );
    }
}
