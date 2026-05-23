//! File-IO for per-agent (`.clank/agents/<label>/config.json`)
//! and repo-level (`.clank/config.json`) typed configs.
//!
//! Pure types live in `clank_core::agent_config`; this module is
//! the only place that knows where they sit on disk and how to
//! load/save them atomically.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;

use clank_core::agent_config::{AgentConfig, RepoConfig};
use clank_core::ids::AgentLabel;

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
