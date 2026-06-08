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

/// Resolve an agent's role from the merged declaration. The
/// declaration is THE source of truth — if `label` isn't in it,
/// the agent isn't registered and gets the default role.
///
/// Pre-Phase-1 (skeleton-only) repos are handled inside
/// [`crate::cli::config::load_merged_agents`]'s level-3 legacy
/// fallback (which synthesizes declaration entries from skeleton
/// dirs). This function does NOT add a second skeleton fallback
/// — that would defeat `clank agent remove`, which deliberately
/// preserves the skeleton dir for feedback history but expects
/// the declaration entry's removal to take effect.
///
/// Codex caught the double-fallback on ebc5d38: a config with
/// `agents: []` + preserved `.clank/agents/codex/config.json`
/// was returning role=master from the skeleton, even though the
/// explicit empty declaration says no agents are registered.
pub fn resolve_role(repo: &Path, label: &AgentLabel) -> anyhow::Result<clank_core::vocab::Role> {
    // Plan: teams-based-agent-registration (codex b59dafb
    // catch — role resolution was left on the legacy path
    // while reviewer tiers cut over to the new resolver. A
    // team master whose legacy `default_agents` entry is
    // `role: reviewer` was getting Role::Reviewer here, so
    // master work items never emitted for them).
    //
    // Dispatch: when the repo config has a `team` field, run
    // the new resolver and derive role from the resolved set
    // (label == master → Master; otherwise Reviewer if in
    // either tier; else default). Falls back to legacy when
    // no team field is set.
    if let Some(set) = try_resolve_via_team(repo)? {
        return Ok(role_from_registered_set(&set, label));
    }
    // Legacy path.
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let merged = crate::cli::config::load_merged_agents(repo, home.as_deref())?;
    Ok(merged
        .iter()
        .find(|e| &e.label == label)
        .map(|e| e.role)
        .unwrap_or_default())
}

/// Labels of every agent registered as a reviewer in this repo.
///
/// Source of truth: the **merged agent declaration** (repo-scope
/// `<repo>/.clank/config.json#/agents` if present, else user-scope
/// `~/.clank/config.json#/default_agents`). Per
/// `agent-add-cli-and-repo-scope`, the declaration drives gate
/// input — `clank agent remove` removing the declaration entry IS
/// what removes a reviewer from the gate.
///
/// **Strict** by design: a malformed declaration file is
/// propagated as an error rather than silently dropping reviewers.
/// The gate's "zero registered reviewers → auto-Approved" rule
/// means lossy loading would fail open — a corrupted declaration
/// could let master finalize without review. Fails closed instead.
pub fn load_expected_reviewers(repo: &Path) -> anyhow::Result<Vec<AgentLabel>> {
    use clank_core::vocab::Role;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let merged = crate::cli::config::load_merged_agents(repo, home.as_deref())?;
    Ok(merged
        .into_iter()
        .filter(|e| e.role == Role::Reviewer)
        .map(|e| e.label)
        .collect())
}

/// Plan: teams-based-agent-registration (phase 6b).
///
/// Two-tier reviewer split for a repo. Used by `WorkPolicy`
/// construction sites (status, wfw, open) to feed the
/// two-tier gate state machine introduced in phase 3.
///
/// Returns `(commit_reviewers, gate_reviewers)`:
/// - If the repo config has a `team` field set, runs
///   `resolve_registered_set` against the new typed schemas.
///   commit_reviewers and gate_reviewers come from the
///   resolved set's reviewer lists.
/// - If `team` is unset (legacy repo), falls back to
///   `load_expected_reviewers` (single list) and returns it
///   as `commit_reviewers` with empty `gate_reviewers` —
///   preserves pre-plan behavior exactly.
///
/// This is the production cutover surface: subsequent
/// phases remove the fallback path entirely.
pub fn load_reviewer_tiers(repo: &Path) -> anyhow::Result<(Vec<AgentLabel>, Vec<AgentLabel>)> {
    match try_resolve_via_team(repo)? {
        Some(set) => {
            let commit = set.commit_reviewers.into_iter().map(|a| a.label).collect();
            let gate = set.gate_reviewers.into_iter().map(|a| a.label).collect();
            Ok((commit, gate))
        }
        None => {
            // No team set or no repo config — legacy single-list
            // path.
            Ok((load_expected_reviewers(repo)?, Vec::new()))
        }
    }
}

/// Plan: teams-based-agent-registration (phase 6b).
///
/// Shared dispatch helper for `load_reviewer_tiers` and
/// `resolve_role`. Returns:
/// - `Ok(Some(RegisteredSet))` when the repo config parses
///   AND has a `team` field. The set is the result of running
///   `resolve_registered_set` against the new typed schemas.
/// - `Ok(None)` when the repo has no `team` field set (or
///   no repo config exists), signaling the caller should fall
///   back to legacy behavior. Per codex b59dafb, this case is
///   narrower than "any parse error" — a present-but-malformed
///   `team` / `promoted` field now FAILS instead of silently
///   falling back.
///
/// Failure modes:
/// - Repo config exists but new-schema parse fails AND the
///   legacy parse can't see it as `default_agents`/`agents`
///   etc.: returns an error (fail-closed). Legacy `agents`
///   array shape lands in `extra` of the new schema (via the
///   flatten catchall), so legacy repos do NOT trigger the
///   parse-fail path — they parse successfully with no
///   `team` field and fall through to `Ok(None)`.
/// - User config doesn't parse as new-schema when team is
///   set: error propagates (fail-closed).
/// - `resolve_registered_set` returns error (UnknownTeam,
///   NoMaster, etc.): error propagates.
/// Plan: teams-based-agent-registration (codex b59dafb pure
/// helper). Derive a role from a resolved registered set.
/// Pure: no $HOME, no filesystem. Testable directly.
fn role_from_registered_set(
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

fn try_resolve_via_team(
    repo: &Path,
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
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let user_cfg: UserConfigFile = match home.as_deref() {
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
}
