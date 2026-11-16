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

    pub(crate) fn storage(&self) -> WatchStorage {
        match &self.legacy_agent {
            Some(agent) => WatchStorage::Agent(agent.clone()),
            None => WatchStorage::Repo,
        }
    }
}

/// Where a watch's logs live. Fixed when the watch is made: retargeting it
/// or a master handoff changes its recipient, never this.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WatchStorage {
    /// `.clank/watch-events/<id>/events`.
    Repo,
    /// The `events/` of the agent whose declaration the watch took over,
    /// where that log already was.
    Agent(AgentLabel),
}

/// Who produces a stream, resolved with where its logs live. Its serial
/// form is tagged, so an agent labelled like a watch's display text is
/// never read back as that watch.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Producer {
    Agent(AgentLabel),
    Watch { id: u64, storage: WatchStorage },
}

impl Producer {
    /// The directory holding this producer's logs. Reads no configuration.
    pub(crate) fn directory(&self, repo: &Path) -> PathBuf {
        match self {
            Self::Agent(agent)
            | Self::Watch {
                storage: WatchStorage::Agent(agent),
                ..
            } => crate::agent_store::agents_root(repo)
                .join(agent.as_str())
                .join("events"),
            Self::Watch {
                id,
                storage: WatchStorage::Repo,
            } => repo
                .join(".clank/watch-events")
                .join(id.to_string())
                .join("events"),
        }
    }

    /// Who this producer's events go to under `config`: an agent itself; a
    /// watch's current recipient, and no one once it is removed.
    pub(crate) fn recipient(&self, config: &RepoConfigFile) -> Option<AgentLabel> {
        match self {
            Self::Agent(agent) => Some(agent.clone()),
            Self::Watch { id, .. } => config
                .event_watches
                .get(id)
                .filter(|watch| !watch.removed)?
                .target
                .recipient(config),
        }
    }
}

impl From<AgentLabel> for Producer {
    fn from(value: AgentLabel) -> Self {
        Self::Agent(value)
    }
}

#[cfg(test)]
impl Producer {
    pub(crate) fn repo_watch(id: u64) -> Self {
        Self::Watch {
            id,
            storage: WatchStorage::Repo,
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Agent(agent) => agent.as_str(),
            Self::Watch { .. } => panic!("{self:?} is not an agent"),
        }
    }
}

#[cfg(test)]
impl From<&str> for Producer {
    fn from(agent: &str) -> Self {
        Self::Agent(AgentLabel::parse(agent).unwrap())
    }
}

impl std::fmt::Display for Producer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Agent(agent) => agent.fmt(f),
            Self::Watch { id, .. } => f.write_str(&watch_text(*id, None)),
        }
    }
}

/// What a hosted source emits under: its producer, and the key of the
/// source within it when the host knows it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Stream {
    pub(crate) producer: Producer,
    pub(crate) key: Option<String>,
}

impl Stream {
    /// The agent this stream is delivered to now. An agent's declaration a
    /// watch has since transferred follows that watch, so an emission in
    /// flight across the transfer reaches the watch's recipient.
    pub(crate) fn recipient(&self, repo: &Path) -> Option<AgentLabel> {
        match (&self.producer, &self.key) {
            (Producer::Agent(agent), None) => Some(agent.clone()),
            (Producer::Agent(agent), Some(key)) => {
                let config = read(repo).ok()?;
                match Watches(&config).log_owner(agent, key) {
                    Producer::Agent(agent) => config.agents.contains_key(&agent).then_some(agent),
                    watch => watch.recipient(&config),
                }
            }
            (watch, _) => watch.recipient(&read(repo).ok()?),
        }
    }
}

impl From<Producer> for Stream {
    fn from(producer: Producer) -> Self {
        Self {
            producer,
            key: None,
        }
    }
}

impl From<AgentLabel> for Stream {
    fn from(agent: AgentLabel) -> Self {
        Producer::Agent(agent).into()
    }
}

/// The text naming watch `id`, or with `key` one of its streams. It is the
/// grammar of `watch:<id>:<source-key>@<seq>` selectors and of the records
/// that name a watch; [`parse_watch_text`] is its only reader.
pub(crate) fn watch_text(id: u64, key: Option<&str>) -> String {
    match key {
        Some(key) => format!("watch:{id}:{key}"),
        None => format!("watch:{id}"),
    }
}

/// [`watch_text`] read back: `None` when `text` names no watch, an error
/// when it names one malformed.
pub(crate) fn parse_watch_text(text: &str) -> Option<anyhow::Result<(u64, Option<&str>)>> {
    let rest = text.strip_prefix("watch:")?;
    let (id, key) = match rest.split_once(':') {
        Some((id, key)) => (id, Some(key)),
        None => (rest, None),
    };
    Some(
        id.parse()
            .map(|id| (id, key))
            .map_err(|e| anyhow::anyhow!("invalid watch `{text}`: {e}")),
    )
}

pub(crate) fn source_key(source: &WaitEventSource) -> String {
    match source {
        WaitEventSource::Github(g) => super::github::supervisor::source_key(g),
        WaitEventSource::Command(c) => c.source_key(),
        WaitEventSource::Timer(t) => super::timer_source::config_key(t),
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static CONFIG_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn read(repo: &Path) -> anyhow::Result<RepoConfigFile> {
    #[cfg(test)]
    CONFIG_READS.with(|reads| reads.set(reads.get() + 1));
    Ok(crate::agent_store::repo_config::read(repo)?.unwrap_or_default())
}

/// What became of an agent's own `wait_events` declaration of one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Legacy {
    /// Hosted from the agent's config.
    Hosted,
    /// Transferred to this watch, removed or not: never hosted from the
    /// config again, and its log is the watch's.
    Transferred(u64),
    /// This watch delivers the same stream to the agent, so it hosts and the
    /// declaration waits until it no longer does.
    Displaced(u64),
}

/// Which producer hosts each stream, decided from one read of the roster.
/// Nothing here is stored: a suppression is what the configuration says
/// now, so the edit that ends a collision lifts it.
#[derive(Clone, Copy)]
pub(crate) struct Watches<'a>(pub(crate) &'a RepoConfigFile);

impl<'a> Watches<'a> {
    pub(crate) fn producer(&self, id: u64) -> Option<Producer> {
        let watch = self.0.event_watches.get(&id)?;
        Some(Producer::Watch {
            id,
            storage: watch.storage(),
        })
    }

    fn live(&self) -> impl Iterator<Item = (u64, &'a Watch)> + use<'a> {
        self.0
            .event_watches
            .iter()
            .filter(|(_, watch)| !watch.removed)
            .map(|(id, watch)| (*id, watch))
    }

    /// The lowest-ID live watch delivering `key` to `recipient`.
    fn delivering(&self, recipient: &AgentLabel, key: &str) -> Option<u64> {
        let config = self.0;
        self.live()
            .find(|(_, watch)| {
                watch.key() == key && watch.target.recipient(config).as_ref() == Some(recipient)
            })
            .map(|(id, _)| id)
    }

    /// The transfer rule: what became of `agent`'s declaration of `key`.
    pub(crate) fn legacy(&self, agent: &AgentLabel, key: &str) -> Legacy {
        if let Some(id) = self.0.event_watches.iter().find_map(|(id, watch)| {
            (watch.legacy_agent.as_ref() == Some(agent) && watch.key() == key).then_some(*id)
        }) {
            return Legacy::Transferred(id);
        }
        match self.delivering(agent, key) {
            Some(id) => Legacy::Displaced(id),
            None => Legacy::Hosted,
        }
    }

    /// Whether `agent`'s own declaration of `source` is hosted from its
    /// config.
    pub(crate) fn hosts_legacy(&self, agent: &AgentLabel, source: &WaitEventSource) -> bool {
        self.legacy(agent, &source_key(source)) == Legacy::Hosted
    }

    /// The producer whose log `agent`'s `events/` holds under `key`.
    pub(crate) fn log_owner(&self, agent: &AgentLabel, key: &str) -> Producer {
        match self.legacy(agent, key) {
            Legacy::Transferred(id) => self.producer(id).expect("a transfer names its own watch"),
            Legacy::Hosted | Legacy::Displaced(_) => Producer::Agent(agent.clone()),
        }
    }

    /// The lower-ID live watch that hosts watch `id`'s stream instead of it,
    /// when both deliver one source key to one recipient. It ingests nothing
    /// new meanwhile; its log, cursor and unacked events stay its own.
    pub(crate) fn displaced_by(&self, id: u64) -> Option<u64> {
        let watch = self.0.event_watches.get(&id).filter(|w| !w.removed)?;
        let recipient = watch.target.recipient(self.0)?;
        self.delivering(&recipient, &watch.key())
            .filter(|winner| *winner != id)
    }

    /// Every live watch that ingests now, with its source.
    pub(crate) fn hosted(&self) -> impl Iterator<Item = (Producer, &'a Watch)> + use<'a> {
        let this = *self;
        self.live()
            .filter(move |(id, _)| this.displaced_by(*id).is_none())
            .map(|(id, watch)| {
                (
                    Producer::Watch {
                        id,
                        storage: watch.storage(),
                    },
                    watch,
                )
            })
    }
}

/// `agent`'s own declarations a watch currently displaces: not hosted, yet
/// their logs stay the agent's, owed and acked under their own keys.
pub(crate) fn displaced_declarations(repo: &Path, agent: &AgentLabel) -> Vec<WaitEventSource> {
    let (Ok(config), Ok(Some(declared))) = (
        read(repo),
        crate::agent_store::load_agent_config(repo, agent),
    ) else {
        return Vec::new();
    };
    let watches = Watches(&config);
    declared
        .wait_events
        .into_iter()
        .filter(|source| {
            matches!(
                watches.legacy(agent, &source_key(source)),
                Legacy::Displaced(_)
            )
        })
        .collect()
}

/// Refuse a watch that would give its recipient a second producer of one
/// source, naming that producer. Rivals share the recipient, or the target
/// while it resolves to no one. `id` is the watch being placed, never its
/// own rival; `legacy_agent`'s declaration of the same source is the one
/// being transferred.
fn ensure_unique(
    repo: &Path,
    config: &RepoConfigFile,
    id: Option<u64>,
    target: &Target,
    source: &WaitEventSource,
    legacy_agent: Option<&AgentLabel>,
) -> anyhow::Result<()> {
    let recipient = target.recipient(config);
    let key = source_key(source);
    let mut rivals: Vec<(Producer, WaitEventSource)> = config
        .event_watches
        .iter()
        .filter(|(other, watch)| {
            Some(**other) != id
                && !watch.removed
                && (&watch.target == target
                    || (recipient.is_some() && watch.target.recipient(config) == recipient))
        })
        .map(|(other, watch)| {
            (
                Producer::Watch {
                    id: *other,
                    storage: watch.storage(),
                },
                watch.source.clone(),
            )
        })
        .collect();
    let declaring = recipient.clone().or(match target {
        Target::Agent { name } => Some(name.clone()),
        Target::Master => None,
    });
    if let Some(agent) = &declaring {
        let watches = Watches(config);
        let declared = crate::agent_store::load_agent_config(repo, agent)?.unwrap_or_default();
        rivals.extend(
            declared
                .wait_events
                .into_iter()
                .filter(|entry| {
                    watches.hosts_legacy(agent, entry)
                        && !(legacy_agent == Some(agent) && source_key(entry) == key)
                })
                .map(|entry| (Producer::Agent(agent.clone()), entry)),
        );
    }
    if let Some((rival, _)) = rivals.iter().find(|(_, other)| source_key(other) == key) {
        let to = declaring.map_or_else(|| target.to_string(), |agent| agent.to_string());
        anyhow::bail!("{to} already receives `{key}` from {rival}");
    }
    let mut sources: Vec<_> = rivals
        .into_iter()
        .map(|(_, source)| (source, SourceOrigin::Config))
        .collect();
    sources.push((source.clone(), SourceOrigin::Config));
    super::wait::validate_wait_event_sources(&sources)
}

fn ensure_on_roster(config: &RepoConfigFile, target: &Target) -> anyhow::Result<()> {
    if let Target::Agent { name } = target {
        anyhow::ensure!(
            config.agents.contains_key(name),
            "agent `{name}` is not on the roster"
        );
    }
    Ok(())
}

fn insert(repo: &Path, config: &mut RepoConfigFile, watch: Watch) -> anyhow::Result<u64> {
    ensure_unique(
        repo,
        config,
        None,
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
        ensure_on_roster(config, &target)?;
        let _binding = match &target {
            Target::Agent { name } => Some(crate::agent_store::hold_state(repo, name)?),
            Target::Master => None,
        };
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

/// Transfer by stable ID to a roster agent or the master role, refused when
/// that recipient already receives the source from another producer.
pub(crate) fn retarget(repo: &Path, id: u64, target: Target) -> anyhow::Result<()> {
    crate::agent_store::repo_config::transact(repo, |config| {
        ensure_on_roster(config, &target)?;
        let _binding = match &target {
            Target::Agent { name } => Some(crate::agent_store::hold_state(repo, name)?),
            Target::Master => None,
        };
        let watch = config
            .event_watches
            .get(&id)
            .filter(|watch| !watch.removed)
            .ok_or_else(|| anyhow::anyhow!("no watch {id}"))?;
        ensure_unique(
            repo,
            config,
            Some(id),
            &target,
            &watch.source,
            watch.legacy_agent.as_ref(),
        )?;
        if let Some(watch) = config.event_watches.get_mut(&id) {
            watch.target = target.clone();
        }
        Ok(())
    })
}

/// The config lock precedes the agent's state lock. No legacy file is rewritten:
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
        ensure_on_roster(config, &target)?;
        let _binding = crate::agent_store::hold_state(repo, agent)?;
        let _target_binding = match &target {
            Target::Agent { name } if name != agent => {
                Some(crate::agent_store::hold_state(repo, name)?)
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
            !matches!(
                Watches(config).legacy(agent, &source_key(&source)),
                Legacy::Transferred(_)
            ),
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

/// Where the stream `source`, as the agent inbox at `inbox` lists it, is
/// stored. A watch selector is checked against its recorded watch, removed
/// or not, before any log is opened; any other source is a log in `inbox`.
pub(crate) fn locate(inbox: &Path, source: &str) -> anyhow::Result<(PathBuf, String)> {
    if parse_watch_text(source).is_none() {
        return Ok((inbox.to_path_buf(), source.to_owned()));
    }
    let (repo, _) = inbox_owner(inbox)
        .ok_or_else(|| anyhow::anyhow!("watch selector outside an agent inbox"))?;
    let (producer, key) = watch_stream(&repo, source)?.expect("parsed as a watch selector");
    Ok((producer.directory(&repo), key))
}

/// The watch and source key the selector `source` names, `None` when it
/// names no watch. Its source key must be the recorded watch's, removed or
/// not.
pub(crate) fn watch_stream(
    repo: &Path,
    source: &str,
) -> anyhow::Result<Option<(Producer, String)>> {
    let Some(parsed) = parse_watch_text(source) else {
        return Ok(None);
    };
    let (id, key) = parsed?;
    let key = key.ok_or_else(|| anyhow::anyhow!("invalid watch selector `{source}`"))?;
    let config = read(repo)?;
    let watch = config
        .event_watches
        .get(&id)
        .ok_or_else(|| anyhow::anyhow!("unknown watch selector `{source}`"))?;
    anyhow::ensure!(
        watch.key() == key,
        "watch selector `{source}` does not match its source"
    );
    Ok(Some((
        Producer::Watch {
            id,
            storage: watch.storage(),
        },
        key.to_owned(),
    )))
}

/// The agent whose inbox `dir` is, which must be in `repo`.
pub(crate) fn inbox_author(repo: &Path, dir: &Path) -> anyhow::Result<AgentLabel> {
    let (inbox_repo, author) =
        inbox_owner(dir).ok_or_else(|| anyhow::anyhow!("invalid inbox directory"))?;
    anyhow::ensure!(
        dunce::canonicalize(inbox_repo)? == dunce::canonicalize(repo)?,
        "inbox belongs to a different repository"
    );
    Ok(author)
}

/// The repository and agent whose inbox `dir` is, when it is
/// `<repo>/.clank/agents/<agent>/events`.
pub(crate) fn inbox_owner(dir: &Path) -> Option<(PathBuf, AgentLabel)> {
    let agent = dir.parent()?;
    let agents = agent.parent()?;
    let clank = agents.parent()?;
    (dir.file_name()? == "events"
        && agents.file_name()? == "agents"
        && clank.file_name()? == ".clank")
        .then_some(())?;
    Some((
        clank.parent()?.to_path_buf(),
        AgentLabel::parse(agent.file_name()?.to_str()?).ok()?,
    ))
}

/// Whether the log `key` in the agent inbox `dir` now belongs to a watch,
/// which presents it under its own selector instead.
pub(crate) fn transferred_away(dir: &Path, key: &str) -> bool {
    let Some((repo, agent)) = inbox_owner(dir) else {
        return false;
    };
    read(&repo).is_ok_and(|config| {
        matches!(
            Watches(&config).log_owner(&agent, key),
            Producer::Watch { .. }
        )
    })
}

pub(crate) fn rows(
    repo: &Path,
    author: &AgentLabel,
) -> anyhow::Result<Vec<super::events::Sourced>> {
    rows_with(repo, author, &super::github::log::Sidecars::waiting())
}

/// [`rows`], each log read as `sidecars` takes its lock. A watch whose
/// sidecar is held contributes no rows and is noted unavailable.
fn rows_with(
    repo: &Path,
    author: &AgentLabel,
    sidecars: &super::github::log::Sidecars,
) -> anyhow::Result<Vec<super::events::Sourced>> {
    use super::github::log::Tried;
    let config = read(repo)?;
    let mut rows = vec![];
    for (id, watch) in assigned(&config, author) {
        let dir = Producer::Watch {
            id,
            storage: watch.storage(),
        }
        .directory(repo);
        let key = watch.key();
        if !dir.join(format!("{key}.jsonl")).is_file() {
            continue;
        }
        let source = watch_text(id, Some(&key));
        let read = (|| {
            let mut log = super::github::log::EventLog::open(&dir, &key)?;
            if matches!(watch.source, WaitEventSource::Timer(_)) {
                sidecars.timer_backlog(&mut log, false, &source)
            } else {
                sidecars.read_rows(&log, &source)
            }
        })();
        let read = match read {
            Ok(Tried::Contended) => continue,
            Ok(Tried::Ran(Some(read))) => read,
            Ok(Tried::Ran(None)) => {
                eprintln!("watch {id} ({key}) has corrupt custody; skipping, repair required");
                continue;
            }
            Err(error) => {
                eprintln!("watch {id} ({key}): {error}; skipping, custody preserved for repair");
                continue;
            }
        };
        rows.extend(read.rows.into_iter().map(|row| super::events::Sourced {
            source: source.clone(),
            row,
        }));
    }
    Ok(rows)
}

pub(crate) fn owed(
    repo: &Path,
    author: &AgentLabel,
    shown: &mut std::collections::HashSet<(String, u64, String)>,
    sidecars: &super::github::log::Sidecars,
) -> Vec<clank_core::wait::WaitItem> {
    let config = match read(repo) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("watch ownership: {e:#}");
            return vec![];
        }
    };
    let rows = match rows_with(repo, author, sidecars) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("watch inbox: {e:#}");
            return vec![];
        }
    };
    let mut owed = vec![];
    for (id, watch) in assigned(&config, author) {
        let key = watch_text(id, Some(&watch.key()));
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

/// A watch's rows under its own source key, for timelines that follow the
/// watch rather than a recipient.
pub(crate) fn producer_rows(
    repo: &Path,
    producer: &Producer,
) -> anyhow::Result<Vec<super::events::Sourced>> {
    let Producer::Watch { id, .. } = producer else {
        let dir = producer.directory(repo);
        return super::events::read_all(&dir);
    };
    let config = read(repo)?;
    let key = config
        .event_watches
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("unknown watch {id}"))?
        .key();
    let location = producer.directory(repo);
    if !location.join(format!("{key}.jsonl")).is_file() {
        return Ok(vec![]);
    }
    let read = super::github::log::EventLog::open(&location, &key)?
        .read_rows()?
        .ok_or_else(|| anyhow::anyhow!("watch {id} has corrupt custody"))?;
    Ok(read
        .rows
        .into_iter()
        .map(|row| super::events::Sourced {
            source: key.clone(),
            row,
        })
        .collect())
}

/// The directory watch `id`'s logs are in, as the roster records it.
#[cfg(test)]
fn dir_of(repo: &Path, id: u64) -> PathBuf {
    Watches(&read(repo).unwrap())
        .producer(id)
        .unwrap()
        .directory(repo)
}

#[cfg(test)]
fn transferred(config: &RepoConfigFile, agent: &AgentLabel, source: &WaitEventSource) -> bool {
    matches!(
        Watches(config).legacy(agent, &source_key(source)),
        Legacy::Transferred(_)
    )
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
        crate::agent_store::repo_config::transact(repo.path(), |cfg| {
            cfg.agents = serde_json::from_value(
                serde_json::json!({"former-master":{"tool":"codex","role":"standby"}}),
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
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
        assert_eq!(dir_of(repo.path(), id), original);
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
        assert_eq!(dir_of(repo.path(), id), original);
        remove(
            repo.path(),
            id,
            &Target::Agent {
                name: agent.clone(),
            },
        )
        .unwrap();
        assert!(transferred(&read(repo.path()).unwrap(), &agent, &source));
        assert!(!Watches(&read(repo.path()).unwrap()).hosts_legacy(&agent, &source));
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
    fn selected(inbox: &Path, source: &str) -> EventLog {
        let (at, key) = locate(inbox, source).unwrap();
        EventLog::open(&at, &key).unwrap()
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
        let mut log = EventLog::open(&dir_of(repo.path(), id), &watch.key()).unwrap();
        let seq = log
            .append_inbox(Transport::Command, Some("one"), None, None, &item("one"))
            .unwrap();
        let selector = format!("{}@{seq}", watch_text(id, Some(&watch.key())));
        roster(repo.path(), Some("beta"));
        assert!(
            super::super::events::read_all(&inbox(repo.path(), "alpha"))
                .unwrap()
                .is_empty()
        );
        cli_ack(repo.path(), "alpha", &selector).await.unwrap();
        assert!(
            owed(
                repo.path(),
                &label("beta"),
                &mut Default::default(),
                &super::super::github::log::Sidecars::waiting()
            )
            .is_empty()
        );
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
        assert!(
            owed(
                repo.path(),
                &label("beta"),
                &mut Default::default(),
                &super::super::github::log::Sidecars::waiting()
            )
            .is_empty()
        );
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
        let mut healthy_log = EventLog::open(&dir_of(repo.path(), healthy), &watch.key()).unwrap();
        let seq = healthy_log
            .append_inbox(Transport::Command, Some("one"), None, None, &item("one"))
            .unwrap();
        let bad = &config.event_watches[&broken];
        std::fs::create_dir_all(dir_of(repo.path(), broken)).unwrap();
        let bad_path = dir_of(repo.path(), broken).join(format!("{}.jsonl", bad.key()));
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
            owed(
                repo.path(),
                &label("alpha"),
                &mut Default::default(),
                &super::super::github::log::Sidecars::waiting()
            )
            .len(),
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
            &format!("{}@{seq}", watch_text(healthy, Some(&watch.key()))),
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
                &format!("{}@1", watch_text(broken, Some(&bad.key())))
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
        let watch_dir = dir_of(repo.path(), id);
        let mut in_flight = EventLog::open(&watch_dir, &watch.key()).unwrap();
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
        let mut acknowledgement = selected(&inbox(repo.path(), "alpha"), &first.source);
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
        assert!(
            owed(
                repo.path(),
                &label("alpha"),
                &mut Default::default(),
                &super::super::github::log::Sidecars::waiting()
            )
            .is_empty()
        );
        let owed = owed(
            repo.path(),
            &label("beta"),
            &mut Default::default(),
            &super::super::github::log::Sidecars::waiting(),
        );
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
            Watches(&cfg).producer(id).unwrap().directory(repo.path()),
            watch_dir
        );
    }

    #[test]
    fn unassigned_watches_keep_ingesting_and_restore_the_same_pending_inbox() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), None);
        let id = add(repo.path(), Target::Master, source()).unwrap();
        let cfg = read(repo.path()).unwrap();
        let watch = &cfg.event_watches[&id];
        EventLog::open(&dir_of(repo.path(), id), &watch.key())
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
        assert_eq!(pending[0].source, watch_text(id, Some(&watch.key())));
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
        let in_flight = Stream {
            producer: Producer::Agent(old.clone()),
            key: Some(source_key(&source)),
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
            owed(
                repo.path(),
                &label("beta"),
                &mut Default::default(),
                &super::super::github::log::Sidecars::waiting()
            )
            .len(),
            1
        );
        let (dir, key) = locate(&inbox(repo.path(), "beta"), &new[0].source).unwrap();
        assert_eq!(dir, inbox(repo.path(), old.as_str()));
        assert_eq!(key, source_key(&source));
        let producer = Watches(&read(repo.path()).unwrap()).producer(id).unwrap();
        let history = producer_rows(repo.path(), &producer).unwrap();
        assert_eq!(history[0].row.ident, new[0].row.ident);
        assert!(
            locate(
                &inbox(repo.path(), "beta"),
                &watch_text(id, Some("../../outside"))
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
        let mut current = selected(&inbox(repo.path(), "beta"), &watch_text(id, Some(&key)));
        let loaded = current.load();
        assert!(loaded.warm);
        assert_eq!(loaded.feed_baselined, Some(true));
        assert_eq!(loaded.etag.as_deref(), Some("etag-before-transfer"));
        assert_eq!(loaded.poll_interval_floor, Some(120));
        assert!(loaded.feed_ids.contains(&"baseline".to_owned()));
        assert!(loaded.feed_ids.contains(&"pending".to_owned()));
        assert_eq!(loaded.unhandled.len(), 1);
        let pending = owed(
            repo.path(),
            &label("beta"),
            &mut Default::default(),
            &super::super::github::log::Sidecars::waiting(),
        );
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
        let spec = vec![(source, SourceOrigin::Config)];
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut timers = super::super::timer_source::TimerSet::new(
            &spec,
            &dir_of(repo.path(), id),
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
        assert!(
            owed(
                repo.path(),
                &label("alpha"),
                &mut Default::default(),
                &super::super::github::log::Sidecars::waiting()
            )
            .is_empty()
        );
        roster(repo.path(), Some("beta"));
        let after = rows(repo.path(), &label("beta")).unwrap();
        assert_eq!(after[0].row.ident, before[0].row.ident);
        assert_eq!(
            owed(
                repo.path(),
                &label("beta"),
                &mut Default::default(),
                &super::super::github::log::Sidecars::waiting()
            )
            .len(),
            1
        );
        let mut log = selected(&inbox(repo.path(), "beta"), &after[0].source);
        log.append_checked_ack(after[0].row.seq, &after[0].row.ident)
            .unwrap();
        drop(timers);
        let mut restarted = super::super::timer_source::TimerSet::new(
            &spec,
            &dir_of(repo.path(), id),
            None,
            tx.into(),
        )
        .unwrap();
        restarted.arm();
        restarted.due();
        assert!(rx.try_recv().is_err());
        assert!(
            owed(
                repo.path(),
                &label("beta"),
                &mut Default::default(),
                &super::super::github::log::Sidecars::waiting()
            )
            .is_empty()
        );
    }
}

#[cfg(test)]
mod producer_tests {
    use super::*;
    use crate::cli::github::log::{EventLog, Transport};
    use clank_core::wait::WaitItem;

    fn label(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
    }
    fn roster(repo: &Path, agents: &[&str], master: &str) {
        crate::agent_store::repo_config::transact(repo, |cfg| {
            cfg.agents = agents
                .iter()
                .map(|name| {
                    let role = if *name == master { "master" } else { "standby" };
                    (
                        label(name),
                        serde_json::from_value(serde_json::json!({"tool":"codex","role":role}))
                            .unwrap(),
                    )
                })
                .collect();
            Ok(())
        })
        .unwrap();
    }
    fn command(name: &str) -> WaitEventSource {
        serde_json::from_value(
            serde_json::json!({"kind":"command", "name":name, "command":["producer"]}),
        )
        .unwrap()
    }
    fn item(id: &str) -> WaitItem {
        serde_json::from_value(serde_json::json!({"kind":"command_event", "name":"build", "id":id, "event":"finished", "content":"result"})).unwrap()
    }
    /// Appends after whatever the log already holds, as its producer does
    /// when it resumes.
    fn append(dir: &Path, key: &str, id: &str) -> u64 {
        let mut log = EventLog::open(dir, key).unwrap();
        log.load();
        log.append_inbox(Transport::Command, Some(id), None, None, &item(id))
            .unwrap()
    }
    fn declare(repo: &Path, agent: &str, source: &WaitEventSource) {
        crate::agent_store::update_agent_config(repo, &label(agent), |cfg| {
            cfg.wait_events.push(source.clone())
        })
        .unwrap();
    }
    fn hosted(repo: &Path) -> Vec<Producer> {
        let config = read(repo).unwrap();
        Watches(&config).hosted().map(|(p, _)| p).collect()
    }
    fn producer(repo: &Path, id: u64) -> Producer {
        Watches(&read(repo).unwrap()).producer(id).unwrap()
    }
    fn owed_to(repo: &Path, agent: &str) -> Vec<WaitItem> {
        owed(
            repo,
            &label(agent),
            &mut Default::default(),
            &super::super::github::log::Sidecars::waiting(),
        )
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
    fn rows_of(dir: &Path, key: &str) -> Vec<super::super::github::log::EventRow> {
        EventLog::open(dir, key)
            .unwrap()
            .read_rows()
            .unwrap()
            .unwrap()
            .rows
    }

    /// The log is opened from where the producer says, with no read of
    /// the roster, and neither a retarget nor a master handoff moves it.
    #[test]
    fn a_transferred_watch_opens_its_agents_log_with_no_config_read() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), &["alpha", "beta", "departed"], "alpha");
        let source = command("build");
        let key = source_key(&source);
        declare(repo.path(), "departed", &source);
        let agent_events = crate::agent_store::agents_root(repo.path())
            .join("departed")
            .join("events");
        let seq = append(&agent_events, &key, "before");
        let id = transfer_legacy(repo.path(), &label("departed"), 1, Target::Master, None).unwrap();
        let transferred = producer(repo.path(), id);
        assert_eq!(
            transferred,
            Producer::Watch {
                id,
                storage: WatchStorage::Agent(label("departed"))
            }
        );

        CONFIG_READS.with(|reads| reads.set(0));
        let log = EventLog::open(&transferred.directory(repo.path()), &key).unwrap();
        assert_eq!(CONFIG_READS.with(std::cell::Cell::get), 0);
        drop(log);
        assert_eq!(transferred.directory(repo.path()), agent_events);

        retarget(
            repo.path(),
            id,
            Target::Agent {
                name: label("beta"),
            },
        )
        .unwrap();
        crate::cli::agent::set_repo_master(repo.path(), &label("beta")).unwrap();
        assert_eq!(producer(repo.path(), id), transferred);
        let after = rows_of(&producer(repo.path(), id).directory(repo.path()), &key);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].seq, seq);
        assert_eq!(
            std::fs::read_dir(repo.path().join(".clank"))
                .unwrap()
                .flatten()
                .filter(|e| e.file_name() == "watch-events")
                .count(),
            0,
            "no second copy of the log was made"
        );
    }

    #[tokio::test]
    async fn a_full_selector_acks_and_a_mismatched_source_key_opens_nothing() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), &["alpha"], "alpha");
        let source = command("build");
        let key = source_key(&source);
        let id = add(repo.path(), Target::Master, source).unwrap();
        let dir = producer(repo.path(), id).directory(repo.path());
        let seq = append(&dir, &key, "one");
        let before: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();

        let wrong = source_key(&command("other"));
        let mismatched = format!("{}@{seq}", watch_text(id, Some(&wrong)));
        let err = cli_ack(repo.path(), "alpha", &mismatched)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("does not match its source"),
            "{err:#}"
        );
        let after: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(before, after, "the mismatched selector opened no log");
        assert!(!rows_of(&dir, &key)[0].acked);

        cli_ack(
            repo.path(),
            "alpha",
            &format!("{}@{seq}", watch_text(id, Some(&key))),
        )
        .await
        .unwrap();
        assert!(rows_of(&dir, &key)[0].acked);
    }

    #[tokio::test]
    async fn an_agent_labelled_like_a_watch_acks_its_own_log_and_never_the_watchs() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), &["alpha", "watch:1"], "alpha");
        let source = command("build");
        let key = source_key(&source);
        let id = add(repo.path(), Target::Master, source).unwrap();
        assert_eq!(id, 1);
        let watch_dir = producer(repo.path(), id).directory(repo.path());
        append(&watch_dir, &key, "watch-row");
        let watch_log = watch_dir.join(format!("{key}.jsonl"));
        let watch_bytes = std::fs::read(&watch_log).unwrap();

        let own = crate::agent_store::agents_root(repo.path())
            .join("watch:1")
            .join("events");
        let seq = append(&own, &key, "own-row");
        let listed = super::super::events::read_all(&own).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].source, key);
        assert_eq!(listed[0].row.ident, rows_of(&own, &key)[0].ident);

        cli_ack(repo.path(), "watch:1", &format!("{key}@{seq}"))
            .await
            .unwrap();
        assert!(rows_of(&own, &key)[0].acked);
        assert_eq!(std::fs::read(&watch_log).unwrap(), watch_bytes);
        assert_eq!(
            owed_to(repo.path(), "alpha").len(),
            1,
            "Watch(1) still owes alpha its own event"
        );
    }

    #[test]
    fn a_master_watch_duplicating_the_masters_own_source_is_refused() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), &["alpha"], "alpha");
        let source = command("build");
        declare(repo.path(), "alpha", &source);
        let err = add(repo.path(), Target::Master, source).unwrap_err();
        assert!(format!("{err:#}").contains("from alpha"), "{err:#}");
        assert!(read(repo.path()).unwrap().event_watches.is_empty());
    }

    #[test]
    fn retargeting_into_a_duplicate_is_refused_and_names_the_other_producer() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), &["alpha", "beta"], "alpha");
        let source = command("build");
        let a = add(repo.path(), Target::Master, source.clone()).unwrap();
        let b = add(
            repo.path(),
            Target::Agent {
                name: label("beta"),
            },
            source,
        )
        .unwrap();
        for target in [
            Target::Master,
            Target::Agent {
                name: label("alpha"),
            },
        ] {
            let err = retarget(repo.path(), b, target).unwrap_err();
            assert!(
                format!("{err:#}").contains(&Producer::repo_watch(a).to_string()),
                "{err:#}"
            );
        }
        assert_eq!(
            read(repo.path()).unwrap().event_watches[&b].target,
            Target::Agent {
                name: label("beta")
            }
        );
    }

    #[test]
    fn retargeting_or_transferring_to_an_off_roster_label_is_refused() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), &["alpha"], "alpha");
        let source = command("build");
        let id = add(repo.path(), Target::Master, source.clone()).unwrap();
        let gone = Target::Agent {
            name: label("gone"),
        };
        assert!(retarget(repo.path(), id, gone.clone()).is_err());
        assert_eq!(
            read(repo.path()).unwrap().event_watches[&id].target,
            Target::Master
        );
        declare(repo.path(), "alpha", &command("other"));
        assert!(transfer_legacy(repo.path(), &label("alpha"), 1, gone, None).is_err());
        assert_eq!(read(repo.path()).unwrap().event_watches.len(), 1);
    }

    #[test]
    fn make_master_onto_an_agent_whose_source_a_master_watch_duplicates_hosts_the_watch() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), &["alpha", "beta"], "alpha");
        let source = command("build");
        declare(repo.path(), "beta", &source);
        let id = add(repo.path(), Target::Master, source.clone()).unwrap();
        crate::cli::agent::set_repo_master(repo.path(), &label("beta")).unwrap();

        assert_eq!(hosted(repo.path()), [producer(repo.path(), id)]);
        assert!(
            crate::cli::wait::resolve_wait_event_sources(repo.path(), &label("beta"), &[])
                .unwrap()
                .is_empty(),
            "beta's own declaration is not hosted beside the watch"
        );
        let configured = crate::cli::source_status::configured(repo.path()).unwrap();
        let declared = configured
            .iter()
            .find(|s| s.agent == Producer::Agent(label("beta")))
            .unwrap();
        assert_eq!(declared.suppressed_by, Some(producer(repo.path(), id)));
        let lines = crate::cli::source_status::status_lines(
            &configured,
            &crate::cli::source_status::Live::Unavailable,
            0,
            false,
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("@beta") && l.contains("suppressed: watch")),
            "{lines:?}"
        );

        crate::cli::agent::set_repo_master(repo.path(), &label("alpha")).unwrap();
        assert_eq!(
            crate::cli::wait::resolve_wait_event_sources(repo.path(), &label("beta"), &[])
                .unwrap()
                .len(),
            1,
            "handing the master on lifts the suppression"
        );
    }

    /// While the master watch hosts beta's stream, beta's own declaration
    /// ingests nothing, but the event its log already holds is still owed
    /// under beta's own key and acks against beta's own log.
    #[tokio::test]
    async fn a_displaced_declarations_existing_event_stays_owed_under_its_own_key() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), &["alpha", "beta"], "alpha");
        let source = command("build");
        let key = source_key(&source);
        declare(repo.path(), "beta", &source);
        let own = crate::agent_store::agents_root(repo.path())
            .join("beta")
            .join("events");
        let seq = append(&own, &key, "beta-pending");
        let id = add(repo.path(), Target::Master, source).unwrap();
        crate::cli::agent::set_repo_master(repo.path(), &label("beta")).unwrap();

        let hosted_sources =
            crate::cli::wait::resolve_wait_event_sources(repo.path(), &label("beta"), &[]).unwrap();
        assert!(
            hosted_sources.is_empty(),
            "nothing new is ingested into beta's own log"
        );
        assert_eq!(hosted(repo.path()), [producer(repo.path(), id)]);
        let owed = |repo: &Path| {
            crate::cli::event_source::owed_from_sources(
                repo,
                &label("beta"),
                &hosted_sources,
                &mut Default::default(),
                &super::super::github::log::Sidecars::waiting(),
            )
        };
        let pending = owed(repo.path());
        assert_eq!(pending.len(), 1);
        let shown = serde_json::to_string(&pending[0]).unwrap();
        assert!(shown.contains(&format!("{key}@{seq}")), "{shown}");
        assert!(parse_watch_text(&key).is_none());

        cli_ack(repo.path(), "beta", &format!("{key}@{seq}"))
            .await
            .unwrap();
        let rows = rows_of(&own, &key);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].acked);
        assert!(owed(repo.path()).is_empty());
    }

    /// Watch A targets the master and B targets beta, one source. Beta
    /// becoming master gives beta both: A hosts, B ingests nothing new,
    /// and B's pending event is still owed and acked under B's own key.
    #[tokio::test]
    async fn a_watch_to_watch_collision_hosts_the_lower_id_and_keeps_the_others_events() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), &["alpha", "beta"], "alpha");
        let source = command("build");
        let key = source_key(&source);
        let a = add(repo.path(), Target::Master, source.clone()).unwrap();
        let b = add(
            repo.path(),
            Target::Agent {
                name: label("beta"),
            },
            source,
        )
        .unwrap();
        let b_dir = producer(repo.path(), b).directory(repo.path());
        let pending = append(&b_dir, &key, "b-pending");
        assert_eq!(hosted(repo.path()).len(), 2);

        crate::cli::agent::set_repo_master(repo.path(), &label("beta")).unwrap();
        assert_eq!(hosted(repo.path()), [producer(repo.path(), a)]);
        let config = read(repo.path()).unwrap();
        assert_eq!(Watches(&config).displaced_by(b), Some(a));
        let reported = crate::cli::source_status::configured(repo.path())
            .unwrap()
            .into_iter()
            .find(|s| s.agent == producer(repo.path(), b))
            .unwrap();
        assert_eq!(reported.suppressed_by, Some(producer(repo.path(), a)));

        let owed = owed_to(repo.path(), "beta");
        assert_eq!(owed.len(), 1);
        let selector = format!("{}@{pending}", watch_text(b, Some(&key)));
        assert!(
            serde_json::to_string(&owed[0])
                .unwrap()
                .contains(&watch_text(b, Some(&key)))
        );
        cli_ack(repo.path(), "beta", &selector).await.unwrap();
        assert!(rows_of(&b_dir, &key)[0].acked);
        let a_dir = producer(repo.path(), a).directory(repo.path());
        assert!(
            !a_dir.join(format!("{key}.jsonl")).exists(),
            "nothing moved into A"
        );

        crate::cli::agent::set_repo_master(repo.path(), &label("alpha")).unwrap();
        assert_eq!(
            hosted(repo.path()).len(),
            2,
            "handing the master on lifts it"
        );
        assert_eq!(
            append(&b_dir, &key, "b-resumed"),
            pending + 1,
            "B resumes its own log"
        );
    }

    #[test]
    fn removing_the_lower_id_winner_lets_the_other_watch_host_from_its_own_cursor() {
        let repo = tempfile::tempdir().unwrap();
        roster(repo.path(), &["alpha", "beta"], "alpha");
        let source = command("build");
        let key = source_key(&source);
        let a = add(repo.path(), Target::Master, source.clone()).unwrap();
        let b = add(
            repo.path(),
            Target::Agent {
                name: label("beta"),
            },
            source,
        )
        .unwrap();
        let b_dir = producer(repo.path(), b).directory(repo.path());
        let last = append(&b_dir, &key, "b-before");
        crate::cli::agent::set_repo_master(repo.path(), &label("beta")).unwrap();
        assert_eq!(hosted(repo.path()), [producer(repo.path(), a)]);

        remove(repo.path(), a, &Target::Master).unwrap();
        assert_eq!(hosted(repo.path()), [producer(repo.path(), b)]);
        assert_eq!(rows_of(&b_dir, &key).len(), 1);
        assert_eq!(append(&b_dir, &key, "b-after"), last + 1);
    }
}
