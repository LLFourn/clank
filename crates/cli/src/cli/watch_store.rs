//! Repository-owned watches. Ownership is independent of the inbox's storage
//! address: transferring a legacy source changes one config document, never
//! copies a live WAL or resets its producer's cursor.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use clank_core::{AgentLabel, agent_config::WaitEventSource};
use serde::{Deserialize, Serialize};

use super::{
    event_source::SourceOrigin,
    teams_config::{RepoConfigFile, RosterRole},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    Master,
    Agent { name: AgentLabel },
}

impl Target {
    pub(crate) fn recipient(&self, config: &RepoConfigFile) -> Option<AgentLabel> {
        match self {
            Self::Agent { name } => config.agents.contains_key(name).then(|| name.clone()),
            Self::Master => {
                let mut masters = config
                    .agents
                    .iter()
                    .filter(|(_, agent)| agent.role == RosterRole::Master)
                    .map(|(name, _)| name.clone());
                let first = masters.next();
                if masters.next().is_some() {
                    None
                } else {
                    first
                }
            }
        }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Master => f.write_str("master"),
            Self::Agent { name } => write!(f, "agent:{name}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watch {
    pub target: Target,
    pub source: WaitEventSource,
    /// Tombstones retain history and prevent a transferred legacy declaration
    /// from being resurrected. IDs are never reused.
    #[serde(default)]
    pub removed: bool,
    /// A transferred watch keeps its exact WAL, sidecars and ingest lease.
    /// This is a storage address, not its recipient.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_agent: Option<AgentLabel>,
}

impl Watch {
    pub(crate) fn key(&self) -> String {
        source_key(&self.source)
    }

    pub(crate) fn directory(&self, repo: &Path, id: u64) -> PathBuf {
        match &self.legacy_agent {
            Some(agent) => crate::agent_store::agents_root(repo)
                .join(agent.as_str())
                .join("events"),
            None => repo
                .join(".clank/watch-events")
                .join(id.to_string())
                .join("events"),
        }
    }
}

pub(crate) fn source_key(source: &WaitEventSource) -> String {
    match source {
        WaitEventSource::Github(g) => super::github::supervisor::source_key(g),
        WaitEventSource::Command(c) => c.source_key(),
        WaitEventSource::Timer(t) => super::timer_source::config_key(t),
    }
}

pub(crate) fn read(repo: &Path) -> anyhow::Result<RepoConfigFile> {
    Ok(crate::agent_store::repo_config::read(repo)?.unwrap_or_default())
}

pub(crate) fn transferred(
    config: &RepoConfigFile,
    agent: &AgentLabel,
    source: &WaitEventSource,
) -> bool {
    let key = source_key(source);
    config
        .event_watches
        .values()
        .any(|watch| watch.legacy_agent.as_ref() == Some(agent) && watch.key() == key)
}

fn validate(
    repo: &Path,
    config: &RepoConfigFile,
    target: &Target,
    source: &WaitEventSource,
    legacy_agent: Option<&AgentLabel>,
) -> anyhow::Result<()> {
    let mut sources: Vec<_> = config
        .event_watches
        .values()
        .filter(|watch| !watch.removed && &watch.target == target)
        .map(|watch| (watch.source.clone(), SourceOrigin::Config))
        .collect();
    if let Target::Agent { name } = target {
        let legacy = crate::agent_store::load_agent_config(repo, name)?.unwrap_or_default();
        sources.extend(
            legacy
                .wait_events
                .into_iter()
                .filter(|entry| {
                    !transferred(config, name, entry)
                        && !(legacy_agent == Some(name) && source_key(entry) == source_key(source))
                })
                .map(|entry| (entry, SourceOrigin::Config)),
        );
    }
    sources.push((source.clone(), SourceOrigin::Config));
    super::wait::validate_wait_event_sources(&sources)
}

fn insert(repo: &Path, config: &mut RepoConfigFile, watch: Watch) -> anyhow::Result<u64> {
    validate(
        repo,
        config,
        &watch.target,
        &watch.source,
        watch.legacy_agent.as_ref(),
    )?;
    let id = config
        .event_watches
        .last_key_value()
        .map_or(Some(1), |(id, _)| id.checked_add(1))
        .ok_or_else(|| anyhow::anyhow!("watch IDs exhausted"))?;
    config.event_watches.insert(id, watch);
    Ok(id)
}

pub(crate) fn add(repo: &Path, target: Target, source: WaitEventSource) -> anyhow::Result<u64> {
    crate::agent_store::repo_config::transact(repo, |config| {
        let mut binding = None;
        if let Target::Agent { name } = &target {
            anyhow::ensure!(
                config.agents.contains_key(name),
                "agent `{name}` is not on the roster"
            );
            binding = Some(crate::agent_store::hold_binding(repo, name)?);
        }
        let _binding = binding;
        insert(
            repo,
            config,
            Watch {
                target: target.clone(),
                source,
                removed: false,
                legacy_agent: None,
            },
        )
    })
}

pub(crate) fn remove(repo: &Path, id: u64, target: &Target) -> anyhow::Result<()> {
    crate::agent_store::repo_config::transact(repo, |config| {
        let watch = config
            .event_watches
            .get_mut(&id)
            .filter(|watch| !watch.removed && &watch.target == target)
            .ok_or_else(|| anyhow::anyhow!("no watch {id} targeting {target}"))?;
        watch.removed = true;
        Ok(())
    })
}

/// Transfer by stable ID. Both the source and target can be unassigned.
pub(crate) fn retarget(repo: &Path, id: u64, target: Target) -> anyhow::Result<()> {
    crate::agent_store::repo_config::transact(repo, |config| {
        let _binding = match &target {
            Target::Agent { name } => Some(crate::agent_store::hold_binding(repo, name)?),
            Target::Master => None,
        };
        let mut watch = config
            .event_watches
            .remove(&id)
            .filter(|watch| !watch.removed)
            .ok_or_else(|| anyhow::anyhow!("no watch {id}"))?;
        validate(
            repo,
            config,
            &target,
            &watch.source,
            watch.legacy_agent.as_ref(),
        )?;
        watch.target = target.clone();
        config.event_watches.insert(id, watch);
        Ok(())
    })
}

/// The config lock precedes the agent binding lock. No legacy file is rewritten:
/// the repository record is the atomic authority that suppresses its old host.
/// A crash cannot leave copied logs or half of a two-document transfer.
pub(crate) fn transfer_legacy(
    repo: &Path,
    agent: &AgentLabel,
    n: usize,
    target: Target,
    expected: Option<&WaitEventSource>,
) -> anyhow::Result<u64> {
    crate::agent_store::repo_config::transact(repo, |config| {
        let _binding = crate::agent_store::hold_binding(repo, agent)?;
        let _target_binding = match &target {
            Target::Agent { name } if name != agent => {
                Some(crate::agent_store::hold_binding(repo, name)?)
            }
            _ => None,
        };
        let legacy = crate::agent_store::load_agent_config(repo, agent)?.unwrap_or_default();
        let source = n
            .checked_sub(1)
            .and_then(|i| legacy.wait_events.get(i))
            .ok_or_else(|| anyhow::anyhow!("{agent} has no legacy source {n}"))?
            .clone();
        anyhow::ensure!(
            expected.is_none_or(|expected| *expected == source),
            "watch {n} changed; list its scope again before transferring"
        );
        anyhow::ensure!(
            !transferred(config, agent, &source),
            "legacy source {n} has already been transferred"
        );
        insert(
            repo,
            config,
            Watch {
                target: target.clone(),
                source,
                removed: false,
                legacy_agent: Some(agent.clone()),
            },
        )
    })
}

pub(crate) fn assigned(config: &RepoConfigFile, author: &AgentLabel) -> BTreeMap<u64, Watch> {
    config
        .event_watches
        .iter()
        .filter(|(_, watch)| {
            !watch.removed && watch.target.recipient(config).as_ref() == Some(author)
        })
        .map(|(id, watch)| (*id, watch.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timer(name: &str) -> WaitEventSource {
        serde_json::from_value(serde_json::json!({"kind":"timer", "name":name,
            "due_at":"2099-01-01T00:00:00Z", "content":"pending work"}))
        .unwrap()
    }

    #[test]
    fn transfer_preserves_storage_and_never_resurrects_a_removed_watch() {
        let repo = tempfile::tempdir().unwrap();
        let agent = AgentLabel::parse("former-master").unwrap();
        let source = timer("deploy");
        crate::agent_store::update_agent_config(repo.path(), &agent, |cfg| {
            cfg.wait_events.push(source.clone())
        })
        .unwrap();
        let original = crate::agent_store::agents_root(repo.path())
            .join(agent.as_str())
            .join("events");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::write(original.join("cursor"), "unchanged").unwrap();
        let id = transfer_legacy(repo.path(), &agent, 1, Target::Master, None).unwrap();
        let config = read(repo.path()).unwrap();
        let watch = &config.event_watches[&id];
        assert_eq!(watch.directory(repo.path(), id), original);
        assert_eq!(watch.source, source);
        assert!(transferred(&config, &agent, &source));
        assert_eq!(watch.target.recipient(&config), None);
        assert_eq!(
            std::fs::read_to_string(original.join("cursor")).unwrap(),
            "unchanged"
        );
        retarget(
            repo.path(),
            id,
            Target::Agent {
                name: agent.clone(),
            },
        )
        .unwrap();
        assert_eq!(
            read(repo.path()).unwrap().event_watches[&id].directory(repo.path(), id),
            original
        );
        remove(
            repo.path(),
            id,
            &Target::Agent {
                name: agent.clone(),
            },
        )
        .unwrap();
        assert!(transferred(&read(repo.path()).unwrap(), &agent, &source));
        assert!(transfer_legacy(repo.path(), &agent, 1, Target::Master, None).is_err());
        let next = add(repo.path(), Target::Master, source).unwrap();
        assert!(next > id);
    }

    #[test]
    fn concurrent_additions_share_the_roster_transaction_and_refuse_duplicates() {
        let repo = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                crate::agent_store::repo_config::transact(repo.path(), |cfg| {
                    cfg.agents = serde_json::from_value(
                        serde_json::json!({"lead":{"tool":"codex","role":"master"}}),
                    )
                    .unwrap();
                    Ok(())
                })
                .unwrap();
            });
            let mut threads = vec![];
            for i in 0..8 {
                let path = repo.path();
                threads.push(scope.spawn(move || {
                    add(path, Target::Master, timer(&format!("timer-{i}"))).unwrap()
                }));
            }
            let ids: std::collections::BTreeSet<_> =
                threads.into_iter().map(|t| t.join().unwrap()).collect();
            assert_eq!(ids.len(), 8);
        });
        assert_eq!(read(repo.path()).unwrap().event_watches.len(), 8);
        assert_eq!(
            Target::Master.recipient(&read(repo.path()).unwrap()),
            Some(AgentLabel::parse("lead").unwrap())
        );
        assert!(add(repo.path(), Target::Master, timer("timer-0")).is_err());
        assert_eq!(read(repo.path()).unwrap().event_watches.len(), 8);
    }

    #[test]
    fn named_add_refuses_an_existing_legacy_source_and_transfer_checks_its_snapshot() {
        let repo = tempfile::tempdir().unwrap();
        let agent = AgentLabel::parse("worker").unwrap();
        crate::agent_store::repo_config::transact(repo.path(), |cfg| {
            cfg.agents = serde_json::from_value(
                serde_json::json!({"worker":{"tool":"codex","role":"master"}}),
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
        let source = timer("existing");
        crate::agent_store::update_agent_config(repo.path(), &agent, |cfg| {
            cfg.wait_events.push(source.clone())
        })
        .unwrap();
        assert!(
            add(
                repo.path(),
                Target::Agent {
                    name: agent.clone()
                },
                source.clone()
            )
            .is_err()
        );
        assert!(read(repo.path()).unwrap().event_watches.is_empty());
        let stale = timer("old-number-one");
        assert!(transfer_legacy(repo.path(), &agent, 1, Target::Master, Some(&stale)).is_err());
        assert!(read(repo.path()).unwrap().event_watches.is_empty());
        transfer_legacy(repo.path(), &agent, 1, Target::Master, Some(&source)).unwrap();
    }
}

/// A producer owner, never a resolved delivery agent. Repository IDs cannot
/// collide with an agent named `master` (or any other agent).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Owner {
    Agent(AgentLabel),
    /// A legacy producer's in-flight emission retains its source address so
    /// a later transfer can route it to the watch's current recipient.
    Source {
        agent: AgentLabel,
        key: String,
    },
    Watch(u64),
}
impl From<AgentLabel> for Owner {
    fn from(value: AgentLabel) -> Self {
        Self::Agent(value)
    }
}
impl std::fmt::Display for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Agent(agent) | Self::Source { agent, .. } => agent.fmt(f),
            Self::Watch(id) => write!(f, "watch:{id}"),
        }
    }
}
impl Owner {
    pub(crate) fn parse(text: &str) -> anyhow::Result<Self> {
        if let Some(id) = text.strip_prefix("watch:") {
            return Ok(Self::Watch(id.parse()?));
        }
        Ok(Self::Agent(
            AgentLabel::parse(text).map_err(anyhow::Error::msg)?,
        ))
    }
    pub(crate) fn recipient(&self, repo: &Path) -> Option<AgentLabel> {
        match self {
            Self::Agent(agent) => Some(agent.clone()),
            Self::Source { agent, key } => {
                let config = read(repo).ok()?;
                match config
                    .event_watches
                    .values()
                    .find(|watch| watch.legacy_agent.as_ref() == Some(agent) && watch.key() == *key)
                {
                    Some(watch) => (!watch.removed)
                        .then(|| watch.target.recipient(&config))
                        .flatten(),
                    None => config.agents.contains_key(agent).then(|| agent.clone()),
                }
            }
            Self::Watch(id) => {
                let config = read(repo).ok()?;
                config
                    .event_watches
                    .get(id)
                    .filter(|watch| !watch.removed)?
                    .target
                    .recipient(&config)
            }
        }
    }
    pub(crate) fn directory(&self, repo: &Path) -> anyhow::Result<PathBuf> {
        match self {
            Self::Agent(agent) | Self::Source { agent, .. } => {
                Ok(crate::agent_store::agents_root(repo)
                    .join(agent.as_str())
                    .join("events"))
            }
            Self::Watch(id) => Ok(read(repo)?
                .event_watches
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("unknown watch {id}"))?
                .directory(repo, *id)),
        }
    }
}

pub(crate) fn selector_key(id: u64, key: &str) -> String {
    format!("watch:{id}:{key}")
}

/// UI history uses the stable watch owner, while CLI inboxes use the agent
/// performing the operation. Storage IDs are never backend reply authors.
pub(crate) fn inbox_author(repo: &Path, dir: &Path) -> anyhow::Result<AgentLabel> {
    let inbox_repo =
        repo_of_events(dir).ok_or_else(|| anyhow::anyhow!("invalid inbox directory"))?;
    anyhow::ensure!(
        dunce::canonicalize(inbox_repo)? == dunce::canonicalize(repo)?,
        "inbox belongs to a different repository"
    );
    let owner = dir
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("cannot identify inbox owner"))?;
    Owner::parse(owner)?
        .recipient(repo)
        .ok_or_else(|| anyhow::anyhow!("this watch has no recipient; assign one before replying"))
}

/// Resolve a repository-watch selector to its unchanged physical log. The
/// namespace is numeric and the key must match the recorded source: a selector
/// cannot be used to select arbitrary paths.
pub(crate) fn log_location(dir: &Path, key: &str) -> std::io::Result<(PathBuf, String)> {
    if let Some(repo) = repo_of_events(dir)
        && let Some(id) = dir
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("watch:"))
    {
        let id: u64 = id.parse().map_err(std::io::Error::other)?;
        let config = read(&repo).map_err(std::io::Error::other)?;
        let watch = config
            .event_watches
            .get(&id)
            .ok_or_else(|| std::io::Error::other("unknown watch"))?;
        if watch.key() != key {
            return Err(std::io::Error::other("watch key mismatch"));
        }
        return Ok((watch.directory(&repo, id), key.to_owned()));
    }
    let Some(rest) = key.strip_prefix("watch:") else {
        return Ok((dir.to_path_buf(), key.to_owned()));
    };
    let (id, source) = rest
        .split_once(':')
        .ok_or_else(|| std::io::Error::other("invalid watch selector"))?;
    let id: u64 = id.parse().map_err(std::io::Error::other)?;
    let repo = repo_of_events(dir)
        .ok_or_else(|| std::io::Error::other("watch selector outside an agent inbox"))?;
    let config = read(&repo).map_err(std::io::Error::other)?;
    let watch = config
        .event_watches
        .get(&id)
        .ok_or_else(|| std::io::Error::other("unknown watch selector"))?;
    if watch.key() != source {
        return Err(std::io::Error::other(
            "watch selector does not match its source",
        ));
    }
    Ok((watch.directory(&repo, id), source.to_owned()))
}

pub(crate) fn log_exists(dir: &Path, key: &str) -> bool {
    log_location(dir, key).is_ok_and(|(dir, key)| dir.join(format!("{key}.jsonl")).is_file())
}

pub(crate) fn repo_of_events(dir: &Path) -> Option<PathBuf> {
    let agents = dir.parent()?.parent()?;
    (dir.file_name()? == "events"
        && agents.file_name()? == "agents"
        && agents.parent()?.file_name()? == ".clank")
        .then(|| agents.parent()?.parent().map(Path::to_path_buf))
        .flatten()
}

pub(crate) fn rows(
    repo: &Path,
    author: &AgentLabel,
) -> anyhow::Result<Vec<super::events::Sourced>> {
    let config = read(repo)?;
    let mut rows = vec![];
    for (id, watch) in assigned(&config, author) {
        let dir = watch.directory(repo, id);
        let key = watch.key();
        if !dir.join(format!("{key}.jsonl")).is_file() {
            continue;
        }
        let read = (|| {
            let mut log = super::github::log::EventLog::open(&dir, &key)?;
            if matches!(watch.source, WaitEventSource::Timer(_)) {
                log.read_timer_backlog(false)
            } else {
                log.read_rows()
            }
        })();
        let read = match read {
            Ok(Some(read)) => read,
            Ok(None) => {
                eprintln!("watch {id} ({key}) has corrupt custody; skipping, repair required");
                continue;
            }
            Err(error) => {
                eprintln!("watch {id} ({key}): {error}; skipping, custody preserved for repair");
                continue;
            }
        };
        rows.extend(read.rows.into_iter().map(|row| super::events::Sourced {
            source: selector_key(id, &key),
            row,
        }));
    }
    Ok(rows)
}

pub(crate) fn owed(
    repo: &Path,
    author: &AgentLabel,
    shown: &mut std::collections::HashSet<(String, u64, String)>,
) -> Vec<clank_core::wait::WaitItem> {
    let config = match read(repo) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("watch ownership: {e:#}");
            return vec![];
        }
    };
    let rows = match rows(repo, author) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("watch inbox: {e:#}");
            return vec![];
        }
    };
    let mut owed = vec![];
    for (id, watch) in assigned(&config, author) {
        let key = selector_key(id, &watch.key());
        let prompt = match &watch.source {
            WaitEventSource::Github(g) => g.prompt.as_deref(),
            _ => None,
        };
        owed.extend(super::command_source::owed_from_log(
            rows.iter()
                .filter(|r| r.source == key)
                .map(|r| r.row.clone())
                .collect(),
            &key,
            prompt,
            shown,
        ));
    }
    owed
}

pub(crate) fn claimed_key(dir: &Path, key: &str) -> bool {
    let Some(repo) = repo_of_events(dir) else {
        return false;
    };
    let Some(author) = dir
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
    else {
        return false;
    };
    read(&repo).is_ok_and(|config| {
        config.event_watches.values().any(|watch| {
            watch
                .legacy_agent
                .as_ref()
                .is_some_and(|a| a.as_str() == author)
                && watch.key() == key
        })
    })
}

/// Timeline owners are stable even when no recipient exists or a watch was
/// removed. Its synthetic inbox path is only an address, never a second store.
pub(crate) fn timeline_rows(dir: &Path) -> anyhow::Result<Option<Vec<super::events::Sourced>>> {
    let Some(repo) = repo_of_events(dir) else {
        return Ok(None);
    };
    let Some(id) = dir
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix("watch:"))
    else {
        return Ok(None);
    };
    let id = id.parse::<u64>()?;
    let config = read(&repo)?;
    let watch = config
        .event_watches
        .get(&id)
        .ok_or_else(|| anyhow::anyhow!("unknown watch {id}"))?;
    let key = watch.key();
    let location = watch.directory(&repo, id);
    if !location.join(format!("{key}.jsonl")).is_file() {
        return Ok(Some(vec![]));
    }
    let read = super::github::log::EventLog::open(&location, &key)?
        .read_rows()?
        .ok_or_else(|| anyhow::anyhow!("watch {id} has corrupt custody"))?;
    Ok(Some(
        read.rows
            .into_iter()
            .map(|row| super::events::Sourced {
                source: key.clone(),
                row,
            })
            .collect(),
    ))
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use crate::cli::github::log::{EventLog, Transport};
    use clank_core::wait::WaitItem;

    fn label(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
    }
    fn roster(repo: &Path, master: Option<&str>) {
        crate::agent_store::repo_config::transact(repo, |cfg| {
            cfg.agents = serde_json::from_value(serde_json::json!({
                "alpha":{"tool":"codex","role":"standby"},
                "beta":{"tool":"codex","role":"standby"},
                "master":{"tool":"codex","role":"standby"}
            }))
            .unwrap();
            if let Some(master) = master {
                cfg.agents.get_mut(&label(master)).unwrap().role = RosterRole::Master;
            }
            Ok(())
        })
        .unwrap();
    }
    fn source() -> WaitEventSource {
        serde_json::from_value(
            serde_json::json!({"kind":"command", "name":"build", "command":["producer"]}),
        )
        .unwrap()
    }
    fn item(id: &str) -> WaitItem {
        serde_json::from_value(serde_json::json!({"kind":"command_event", "name":"build", "id":id, "event":"finished", "content":"result"})).unwrap()
    }
    fn inbox(repo: &Path, author: &str) -> PathBuf {
        crate::agent_store::agents_root(repo)
            .join(author)
            .join("events")
    }

    async fn cli_ack(repo: &Path, author: &str, selector: &str) -> anyhow::Result<()> {
        use crate::cli::command::{Cli, Command};
        use clap::Parser;
        let Command::Events(args) = Cli::try_parse_from([
            "clank",
            "events",
            "ack",
            "--repo",
            repo.to_str().unwrap(),
            "--author",
            author,
            selector,
        ])?
        .command
        else {
            panic!("events command")
        };
        super::super::events::run(args).await
    }

    #[tokio::test]
    async fn issued_selectors_remain_acknowledgeable_after_handover_and_transfer() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), Some("alpha"));
        let id = add(repo.path(), Target::Master, source()).unwrap();
        let config = read(repo.path()).unwrap();
        let watch = &config.event_watches[&id];
        let mut log = EventLog::open(&watch.directory(repo.path(), id), &watch.key()).unwrap();
        let seq = log
            .append_inbox(Transport::Command, Some("one"), None, None, &item("one"))
            .unwrap();
        let selector = format!("{}@{seq}", selector_key(id, &watch.key()));
        roster(repo.path(), Some("beta"));
        assert!(
            super::super::events::read_all(&inbox(repo.path(), "alpha"))
                .unwrap()
                .is_empty()
        );
        cli_ack(repo.path(), "alpha", &selector).await.unwrap();
        assert!(owed(repo.path(), &label("beta"), &mut Default::default()).is_empty());
        assert!(log.read_rows().unwrap().unwrap().rows[0].acked);

        // An old plain selector works for both the original agent and the new
        // recipient, without copying or rediscovering that log in the old inbox.
        remove(repo.path(), id, &Target::Master).unwrap();
        let legacy = source();
        crate::agent_store::update_agent_config(repo.path(), &label("alpha"), |cfg| {
            cfg.wait_events.push(legacy.clone());
        })
        .unwrap();
        let key = source_key(&legacy);
        let mut log = EventLog::open(&inbox(repo.path(), "alpha"), &key).unwrap();
        let first = log
            .append_inbox(
                Transport::Command,
                Some("legacy-one"),
                None,
                None,
                &item("legacy-one"),
            )
            .unwrap();
        let second = log
            .append_inbox(
                Transport::Command,
                Some("legacy-two"),
                None,
                None,
                &item("legacy-two"),
            )
            .unwrap();
        transfer_legacy(repo.path(), &label("alpha"), 1, Target::Master, None).unwrap();
        cli_ack(repo.path(), "alpha", &format!("{key}@{first}"))
            .await
            .unwrap();
        cli_ack(repo.path(), "beta", &format!("{key}@{second}"))
            .await
            .unwrap();
        assert!(
            log.read_rows()
                .unwrap()
                .unwrap()
                .rows
                .iter()
                .all(|r| r.acked)
        );
        assert!(
            super::super::events::read_all(&inbox(repo.path(), "alpha"))
                .unwrap()
                .is_empty()
        );
        assert!(owed(repo.path(), &label("beta"), &mut Default::default()).is_empty());
    }

    #[tokio::test]
    async fn corrupt_watch_does_not_hide_healthy_delivery_or_prevent_cli_ack() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), Some("alpha"));
        let healthy = add(repo.path(), Target::Master, source()).unwrap();
        let broken_source = serde_json::from_value(serde_json::json!({
            "kind":"command", "name":"broken", "command":["producer"]
        }))
        .unwrap();
        let broken = add(repo.path(), Target::Master, broken_source).unwrap();
        let config = read(repo.path()).unwrap();
        let watch = &config.event_watches[&healthy];
        let mut healthy_log =
            EventLog::open(&watch.directory(repo.path(), healthy), &watch.key()).unwrap();
        let seq = healthy_log
            .append_inbox(Transport::Command, Some("one"), None, None, &item("one"))
            .unwrap();
        let bad = &config.event_watches[&broken];
        std::fs::create_dir_all(bad.directory(repo.path(), broken)).unwrap();
        let bad_path = bad
            .directory(repo.path(), broken)
            .join(format!("{}.jsonl", bad.key()));
        let corrupt = b"this is not a WAL record\n";
        std::fs::write(&bad_path, corrupt).unwrap();
        let mut local_log = EventLog::open(&inbox(repo.path(), "alpha"), "local").unwrap();
        let local_seq = local_log
            .append_inbox(
                Transport::Command,
                Some("local"),
                None,
                None,
                &item("local"),
            )
            .unwrap();
        assert_eq!(
            owed(repo.path(), &label("alpha"), &mut Default::default()).len(),
            1
        );
        assert_eq!(
            super::super::events::read_all(&inbox(repo.path(), "alpha"))
                .unwrap()
                .len(),
            2
        );
        cli_ack(
            repo.path(),
            "alpha",
            &format!("{}@{seq}", selector_key(healthy, &watch.key())),
        )
        .await
        .unwrap();
        cli_ack(repo.path(), "alpha", &format!("local@{local_seq}"))
            .await
            .unwrap();
        assert!(healthy_log.read_rows().unwrap().unwrap().rows[0].acked);
        assert!(local_log.read_rows().unwrap().unwrap().rows[0].acked);
        assert_eq!(std::fs::read(&bad_path).unwrap(), corrupt);
        assert!(
            cli_ack(
                repo.path(),
                "alpha",
                &format!("{}@1", selector_key(broken, &bad.key()))
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read(&bad_path).unwrap(), corrupt);
    }

    #[test]
    fn pending_and_in_flight_work_follow_the_role_without_replaying_acknowledged_work() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), Some("alpha"));
        let id = add(repo.path(), Target::Master, source()).unwrap();
        let pinned = add(
            repo.path(),
            Target::Agent {
                name: label("master"),
            },
            source(),
        )
        .unwrap();
        let cfg = read(repo.path()).unwrap();
        let watch = &cfg.event_watches[&id];
        let mut in_flight =
            EventLog::open(&watch.directory(repo.path(), id), &watch.key()).unwrap();
        in_flight
            .append_inbox(
                Transport::Command,
                Some("handled"),
                None,
                None,
                &item("handled"),
            )
            .unwrap();
        in_flight
            .append_inbox(
                Transport::Command,
                Some("pending"),
                None,
                None,
                &item("pending"),
            )
            .unwrap();
        let before = rows(repo.path(), &label("alpha")).unwrap();
        let first = &before[0];
        let mut acknowledgement =
            EventLog::open(&inbox(repo.path(), "alpha"), &first.source).unwrap();
        // The ack handle and producer already exist when the roster changes.
        roster(repo.path(), Some("beta"));
        acknowledgement
            .append_checked_ack(first.row.seq, &first.row.ident)
            .unwrap();
        in_flight
            .append_inbox(
                Transport::Command,
                Some("in-flight"),
                None,
                None,
                &item("in-flight"),
            )
            .unwrap();
        assert!(owed(repo.path(), &label("alpha"), &mut Default::default()).is_empty());
        let owed = owed(repo.path(), &label("beta"), &mut Default::default());
        assert_eq!(owed.len(), 2);
        assert_eq!(
            rows(repo.path(), &label("beta"))
                .unwrap()
                .iter()
                .filter(|r| r.row.acked)
                .count(),
            1
        );
        let cfg = read(repo.path()).unwrap();
        assert!(assigned(&cfg, &label("master")).contains_key(&pinned));
        assert!(!assigned(&cfg, &label("beta")).contains_key(&pinned));
        assert_eq!(
            cfg.event_watches[&id].directory(repo.path(), id),
            watch.directory(repo.path(), id)
        );
    }

    #[test]
    fn unassigned_watches_keep_ingesting_and_restore_the_same_pending_inbox() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), None);
        let id = add(repo.path(), Target::Master, source()).unwrap();
        let cfg = read(repo.path()).unwrap();
        let watch = &cfg.event_watches[&id];
        EventLog::open(&watch.directory(repo.path(), id), &watch.key())
            .unwrap()
            .append_inbox(Transport::Command, Some("one"), None, None, &item("one"))
            .unwrap();
        assert!(rows(repo.path(), &label("alpha")).unwrap().is_empty());
        let sources = crate::cli::source_status::configured(repo.path()).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].target, Some(Target::Master));
        assert_eq!(sources[0].recipient, None);
        roster(repo.path(), Some("alpha"));
        let pending = rows(repo.path(), &label("alpha")).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].source, selector_key(id, &watch.key()));
        let presented = clank_core::wait::EventPresentation {
            source: pending[0].source.clone(),
            seq: pending[0].row.seq,
            ident: pending[0].row.ident.clone(),
        };
        crate::cli::presentation::record(
            repo.path(),
            &label("alpha"),
            &[presented],
            || true,
            &crate::upgrade::process().settling(),
        );
        assert!(
            rows(repo.path(), &label("alpha")).unwrap()[0]
                .row
                .woken_at
                .is_some()
        );
        let sources = crate::cli::source_status::configured(repo.path()).unwrap();
        assert_eq!(sources[0].recipient, Some(label("alpha")));
    }

    #[test]
    fn legacy_transfer_preserves_pending_and_acked_rows_and_hides_the_old_inbox() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), Some("beta"));
        let old = label("removed-agent");
        let source = source();
        crate::agent_store::update_agent_config(repo.path(), &old, |cfg| {
            cfg.wait_events.push(source.clone())
        })
        .unwrap();
        let mut log =
            EventLog::open(&inbox(repo.path(), old.as_str()), &source_key(&source)).unwrap();
        log.append_inbox(Transport::Command, Some("one"), None, None, &item("one"))
            .unwrap();
        let handled = log
            .append_inbox(
                Transport::Command,
                Some("handled"),
                None,
                None,
                &item("handled"),
            )
            .unwrap();
        log.append_ack(handled).unwrap();
        let id = transfer_legacy(repo.path(), &old, 1, Target::Master, None).unwrap();
        let in_flight = Owner::Source {
            agent: old.clone(),
            key: source_key(&source),
        };
        assert_eq!(in_flight.recipient(repo.path()), Some(label("beta")));
        assert!(
            super::super::events::read_all(&inbox(repo.path(), old.as_str()))
                .unwrap()
                .is_empty()
        );
        assert!(
            super::super::wait::resolve_wait_event_sources(repo.path(), &old, &[])
                .unwrap()
                .is_empty()
        );
        let new = super::super::events::read_all(&inbox(repo.path(), "beta")).unwrap();
        assert_eq!(new.len(), 2);
        assert_eq!(new.iter().filter(|r| r.row.acked).count(), 1);
        assert_eq!(
            owed(repo.path(), &label("beta"), &mut Default::default()).len(),
            1
        );
        let (dir, key) = log_location(&inbox(repo.path(), "beta"), &new[0].source).unwrap();
        assert_eq!(dir, inbox(repo.path(), old.as_str()));
        assert_eq!(key, source_key(&source));
        let history = timeline_rows(&inbox(repo.path(), &Owner::Watch(id).to_string()))
            .unwrap()
            .unwrap();
        assert_eq!(history[0].row.ident, new[0].row.ident);
        assert!(
            log_location(
                &inbox(repo.path(), "beta"),
                &format!("watch:{id}:../../outside")
            )
            .is_err()
        );
    }

    #[test]
    fn github_transfer_and_handover_keep_the_poll_cursor_and_baseline() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), Some("alpha"));
        let old = label("departed");
        let source: WaitEventSource = serde_json::from_value(serde_json::json!({
            "kind":"github", "repo":"owner/repo", "events":["pr_opened"], "prompt":"triage only"
        }))
        .unwrap();
        crate::agent_store::update_agent_config(repo.path(), &old, |cfg| {
            cfg.wait_events.push(source.clone())
        })
        .unwrap();
        let key = source_key(&source);
        let mut log = EventLog::open(&inbox(repo.path(), old.as_str()), &key).unwrap();
        let event: WaitItem = serde_json::from_value(serde_json::json!({
            "kind":"github_event", "repo":"owner/repo", "event":"pr_opened", "number":7
        }))
        .unwrap();
        log.append_baseline_inbox(Some("baseline"), None, None, &event)
            .unwrap();
        log.append_inbox(Transport::Poll, Some("pending"), None, None, &event)
            .unwrap();
        log.save_state(Some("etag-before-transfer"), Some(120), true)
            .unwrap();
        let id = transfer_legacy(repo.path(), &old, 1, Target::Master, None).unwrap();
        roster(repo.path(), Some("beta"));
        let mut current =
            EventLog::open(&inbox(repo.path(), "beta"), &selector_key(id, &key)).unwrap();
        let loaded = current.load();
        assert!(loaded.warm);
        assert_eq!(loaded.feed_baselined, Some(true));
        assert_eq!(loaded.etag.as_deref(), Some("etag-before-transfer"));
        assert_eq!(loaded.poll_interval_floor, Some(120));
        assert!(loaded.feed_ids.contains(&"baseline".to_owned()));
        assert!(loaded.feed_ids.contains(&"pending".to_owned()));
        assert_eq!(loaded.unhandled.len(), 1);
        let pending = owed(repo.path(), &label("beta"), &mut Default::default());
        assert!(
            matches!(&pending[0], WaitItem::GithubEvent { instructions: Some(prompt), .. } if prompt == "triage only")
        );
    }

    #[tokio::test]
    async fn a_pending_timer_keeps_its_occurrence_through_handover_and_ack() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), Some("alpha"));
        let source: WaitEventSource = serde_json::from_value(serde_json::json!({
            "kind":"timer", "name":"due", "due_at":"2020-01-01T00:00:00Z", "content":"check result"
        }))
        .unwrap();
        let id = add(repo.path(), Target::Master, source.clone()).unwrap();
        let cfg = read(repo.path()).unwrap();
        let watch = &cfg.event_watches[&id];
        let spec = vec![(source, SourceOrigin::Config)];
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut timers = super::super::timer_source::TimerSet::new(
            &spec,
            &watch.directory(repo.path(), id),
            None,
            tx.clone().into(),
        )
        .unwrap();
        timers.arm();
        timers.due();
        assert!(matches!(
            rx.try_recv().unwrap().0,
            WaitItem::TimerEvent { .. }
        ));
        let before = rows(repo.path(), &label("alpha")).unwrap();
        roster(repo.path(), None);
        assert!(owed(repo.path(), &label("alpha"), &mut Default::default()).is_empty());
        roster(repo.path(), Some("beta"));
        let after = rows(repo.path(), &label("beta")).unwrap();
        assert_eq!(after[0].row.ident, before[0].row.ident);
        assert_eq!(
            owed(repo.path(), &label("beta"), &mut Default::default()).len(),
            1
        );
        let mut log = EventLog::open(&inbox(repo.path(), "beta"), &after[0].source).unwrap();
        log.append_checked_ack(after[0].row.seq, &after[0].row.ident)
            .unwrap();
        drop(timers);
        let mut restarted = super::super::timer_source::TimerSet::new(
            &spec,
            &watch.directory(repo.path(), id),
            None,
            tx.into(),
        )
        .unwrap();
        restarted.arm();
        restarted.due();
        assert!(rx.try_recv().is_err());
        assert!(owed(repo.path(), &label("beta"), &mut Default::default()).is_empty());
    }
}
