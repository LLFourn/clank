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

use clank_core::agent_config::{AgentConfig, RepoConfig, Session};
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

/// Repo-relative path: `.clank/config.json`.
pub fn repo_config_path(repo: &Path) -> PathBuf {
    repo.join(".clank").join("config.json")
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

/// Load `.clank/config.json` (repo-level). `Ok(None)` if missing.
pub fn load_repo_config(repo: &Path) -> anyhow::Result<Option<RepoConfig>> {
    let path = repo_config_path(repo);
    load_json(&path).with_context(|| format!("reading `{}`", path.display()))
}

/// Atomically write `.clank/config.json`.
pub fn save_repo_config(repo: &Path, cfg: &RepoConfig) -> anyhow::Result<()> {
    let path = repo_config_path(repo);
    save_json(&path, cfg).with_context(|| format!("writing `{}`", path.display()))
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
