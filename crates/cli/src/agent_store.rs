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
    let home = std::env::var_os("HOME").map(PathBuf::from);
    load_reviewer_tiers_with(repo, home.as_deref())
}

/// Home-explicit [`load_reviewer_tiers`] — for in-process callers
/// (tests, query cores) that supply the home dir rather than
/// reading `$HOME`. Plan: dogfood-init-setup-in-tests (Phase B).
pub fn load_reviewer_tiers_with(
    repo: &Path,
    home: Option<&Path>,
) -> anyhow::Result<(Vec<AgentLabel>, Vec<AgentLabel>)> {
    match try_resolve_via_team_with(repo, home)? {
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
    let home = std::env::var_os("HOME").map(PathBuf::from);
    reviewer_tiers_for_render_with(repo, home.as_deref())
}

/// Home-explicit [`reviewer_tiers_for_render`] — for in-process
/// render cores that supply the home dir. Same never-errors
/// degrade semantics. Plan: dogfood-init-setup-in-tests (Phase B).
pub fn reviewer_tiers_for_render_with(
    repo: &Path,
    home: Option<&Path>,
) -> (Vec<AgentLabel>, Vec<AgentLabel>) {
    match try_resolve_via_team_with(repo, home) {
        Ok(Some(set)) => (
            set.commit_reviewers.into_iter().map(|a| a.label).collect(),
            set.gate_reviewers.into_iter().map(|a| a.label).collect(),
        ),
        _ => (Vec::new(), Vec::new()),
    }
}

/// Derive a role from a resolved registered set. Pure: no
/// $HOME, no filesystem. Testable directly.
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

/// Shared dispatch helper for `load_reviewer_tiers`,
/// `reviewer_tiers_for_render`, and `resolve_role`. Returns:
/// - `Ok(Some(RegisteredSet))` when the repo config parses and
///   resolves (a master is set).
/// - `Ok(None)` when there's no repo config OR the repo has no
///   master yet (`ResolutionError::NoMaster`). This is "no team
///   configured" — workflow commands error
///   (`no_team_configured()`), read-only renderers degrade to
///   empty tiers.
/// - `Err(_)` (fail-closed) when the config is malformed, uses
///   the old team schema (re-init hint), or resolution fails for
///   any reason other than `NoMaster` (e.g. `UnknownAgent`).
///
/// The `home` parameter is unused: the repo config is now
/// self-contained (it carries its own `agents`). It's kept so
/// the many workflow callers need no signature changes.
pub fn try_resolve_via_team_with(
    repo: &Path,
    _home: Option<&Path>,
) -> anyhow::Result<Option<crate::cli::teams_config::RegisteredSet>> {
    use crate::cli::teams_config::{ResolutionError, resolve_registered_set};

    let repo_cfg_path = repo.join(".clank/config.json");
    let Some(repo_cfg) = load_repo_config(&repo_cfg_path)? else {
        return Ok(None);
    };

    // The repo is self-contained: master + reviewers reference
    // labels in the repo's own `agents`. A bootstrapped repo
    // (no master yet) resolves to `NoMaster` — surfaced as
    // `Ok(None)` so workflow callers report "no team configured"
    // and read-only renderers degrade to empty tiers, rather
    // than every command erroring before the user has set a
    // master.
    match resolve_registered_set(&repo_cfg) {
        Ok(set) => Ok(Some(set)),
        Err(ResolutionError::NoMaster) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Load + parse `<repo>/.clank/config.json` as the new-shape
/// [`RepoConfigFile`]. Returns `Ok(None)` if the file doesn't
/// exist. Fail-closed: an old-shape config (legacy `team:
/// "name"` / `team: [array]` / `promoted`) no longer parses as
/// `TeamComposition`/`agents`, so rather than surface a cryptic
/// serde message we map it to an actionable re-init hint.
fn load_repo_config(
    repo_cfg_path: &Path,
) -> anyhow::Result<Option<crate::cli::teams_config::RepoConfigFile>> {
    use crate::cli::teams_config::RepoConfigFile;
    let body = match std::fs::read_to_string(repo_cfg_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    // Fail-closed BEFORE accepting the parse: a config can parse yet
    // still carry a legacy marker the typed struct silently swallows —
    // a stray `promoted` lands in the `extra` flatten map and would be
    // ignored (codex c1e6749). Checking the raw shape first means any
    // legacy marker yields the re-init hint, parse-success or not.
    if is_legacy_repo_shape(&body) {
        return Err(legacy_repo_schema_error(repo_cfg_path));
    }
    serde_json::from_str::<RepoConfigFile>(&body)
        .map(Some)
        .map_err(|e| {
            anyhow::anyhow!(
                "parsing {} as new-schema RepoConfigFile: {e}",
                repo_cfg_path.display()
            )
        })
}

/// Detect the old repo schema by raw shape. The new schema has no
/// `promoted`, stores `team` as an object ([`TeamComposition`]), and
/// `agents` as a map — so a `promoted` key, a string/array `team`, or
/// an array `agents` is unambiguously legacy. Checked on the raw JSON
/// (not the typed struct) so a marker the typed parse would swallow
/// into `extra` (e.g. `promoted`) still fail-closes.
fn is_legacy_repo_shape(body: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let Some(obj) = value.as_object() else {
        return false;
    };
    obj.contains_key("promoted")
        || matches!(obj.get("team"), Some(t) if t.is_string() || t.is_array())
        || matches!(obj.get("agents"), Some(a) if a.is_array())
}

fn legacy_repo_schema_error(repo_cfg_path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "{}: this repo's `.clank/config.json` uses the old team schema; \
         re-run `clank init` to recreate it",
        repo_cfg_path.display()
    )
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

    use tempfile::TempDir;

    fn write_repo_config(repo: &Path, body: &str) {
        let p = repo.join(".clank/config.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
    }

    #[test]
    fn try_resolve_via_team_with_returns_none_when_repo_config_missing() {
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let r = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap();
        assert!(r.is_none(), "no repo config → None (no team configured)");
    }

    #[test]
    fn try_resolve_via_team_with_returns_none_when_empty_config() {
        // Bootstrapped repo (empty agents, default team) has no
        // master yet → None (no team configured).
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(repo.path(), "{}");
        let r = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap();
        assert!(r.is_none(), "empty config → None (no master yet)");
    }

    #[test]
    fn try_resolve_via_team_with_returns_set_when_team_resolves() {
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(
            repo.path(),
            r#"{
                "agents": {
                    "codex": { "tool": "codex" },
                    "claude": { "tool": "claude" }
                },
                "team": { "master": "codex", "commit_reviewers": ["claude"] }
            }"#,
        );
        let set = try_resolve_via_team_with(repo.path(), Some(home.path()))
            .unwrap()
            .expect("master set → Some(set)");
        assert_eq!(set.master.as_str(), "codex");
        assert_eq!(set.commit_reviewers.len(), 1);
        assert_eq!(set.commit_reviewers[0].label.as_str(), "claude");
    }

    #[test]
    fn try_resolve_via_team_with_propagates_resolver_errors() {
        // Master references an agent not defined in the repo's
        // `agents` → ResolutionError::UnknownAgent propagates.
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(repo.path(), r#"{"team": {"master": "phantom"}}"#);
        let err = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("phantom") || msg.contains("not defined"),
            "expected UnknownAgent error; got: {msg}"
        );
    }

    #[test]
    fn try_resolve_via_team_with_fails_closed_on_old_team_string_shape() {
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(repo.path(), r#"{"team": "dev"}"#);
        let err = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("old team schema") && msg.contains("clank init"),
            "expected re-init hint; got: {msg}"
        );
    }

    #[test]
    fn try_resolve_via_team_with_fails_closed_on_old_promoted_shape() {
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(
            repo.path(),
            r#"{"team": [{"include": "dev"}], "promoted": "codex"}"#,
        );
        let err = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("old team schema") && msg.contains("clank init"),
            "expected re-init hint; got: {msg}"
        );
    }

    #[test]
    fn try_resolve_via_team_with_fails_closed_on_parseable_promoted() {
        // codex c1e6749: a config with a VALID new-shape `team` object
        // PLUS a stray `promoted` parses fine (promoted → `extra`), so
        // checking legacy shape only on parse-failure silently swallowed
        // it. It must fail closed with the re-init hint.
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(
            repo.path(),
            r#"{"agents": {"claude": {"tool": "claude"}}, "team": {"master": "claude"}, "promoted": "codex"}"#,
        );
        let err = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("old team schema") && msg.contains("clank init"),
            "expected re-init hint; got: {msg}"
        );
    }

    #[test]
    fn try_resolve_via_team_with_fails_closed_on_legacy_agents_array() {
        // A pre-team `agents` array (old shape) → re-init hint, not a
        // cryptic serde error.
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(repo.path(), r#"{"agents": ["claude", "codex"]}"#);
        let err = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("old team schema") && msg.contains("clank init"),
            "expected re-init hint; got: {msg}"
        );
    }
}
