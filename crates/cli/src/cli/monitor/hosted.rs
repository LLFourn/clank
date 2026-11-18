use clank_core::ids::AgentLabel;
use clank_core::wait::WaitItem;

use crate::cli::watch_store::{Producer, Stream};

use super::retire::OwnedTasks;
use super::wire::Control;

mod owed;

pub(super) use self::owed::{admissions, identities};
use self::owed::{admitted_items, gather, owed_items, owed_now};

/// The sink `author`'s hosted sources emit into, reporting their
/// lifecycle when the monitor publishes it.
fn host_sink(
    into: &tokio::sync::mpsc::UnboundedSender<(Stream, crate::cli::event_source::Emission)>,
    lifecycle: Option<&LifecycleTx>,
    author: &Producer,
    instance: u64,
    sources: &[clank_core::agent_config::WaitEventSource],
) -> crate::cli::event_source::EmissionSink {
    let owner = author.clone();
    let sources = sources.to_vec();
    let sink = crate::cli::event_source::EmissionSink::routed(into.clone(), move |item| {
        use clank_core::agent_config::WaitEventSource;
        let source = sources.iter().find(|source| match (source, item) {
            (WaitEventSource::Command(s), WaitItem::CommandEvent { name, .. }) => s.name == *name,
            (WaitEventSource::Timer(s), WaitItem::TimerEvent { name, .. }) => s.name == *name,
            (WaitEventSource::Github(s), WaitItem::GithubEvent { repo, .. }) => s.repo == *repo,
            _ => false,
        });
        Stream {
            producer: owner.clone(),
            key: source.map(crate::cli::watch_store::source_key),
        }
    });
    match lifecycle {
        Some(tx) => sink.reporting(tx.clone(), author.clone(), instance),
        None => sink,
    }
}

/// The account of what the monitor hosts, from `hosted` and the last
/// lifecycle each running source reported.
fn sources_account(
    hosted: &std::collections::BTreeMap<Producer, Hosted>,
    lifecycles: &Lifecycles,
    birth: crate::proc_identity::ProcToken,
) -> crate::cli::source_status::Account {
    use crate::cli::source_status::{Hosted as Entry, fingerprint};
    let sources = hosted
        .iter()
        .flat_map(|(author, host)| {
            host.spec.iter().map(move |(config, _)| {
                let key = crate::cli::watch_store::source_key(config);
                Entry {
                    agent: author.clone(),
                    lifecycle: lifecycles
                        .get(&(author.clone(), key.clone()))
                        .filter(|(instance, _)| current(hosted, author, &key, *instance))
                        .map(|(_, state)| state.clone()),
                    key,
                    accepted: fingerprint(config),
                    foreign: None,
                }
            })
        })
        .collect();
    crate::cli::source_status::Account {
        monitor: std::process::id() as i32,
        birth,
        sources,
    }
}

/// Publish the account when it says something new: whether it did, so
/// the observers that show it are woken. A rewrite of the same account
/// would refresh every panel for nothing.
fn publish_sources(
    repo: &std::path::Path,
    account: crate::cli::source_status::Account,
    last: &mut Option<crate::cli::source_status::Account>,
) -> bool {
    if last.as_ref() == Some(&account) {
        return false;
    }
    let published = crate::upgrade::process()
        .permit()
        .map_err(std::io::Error::from)
        .and_then(|permit| crate::cli::source_status::publish(repo, &account, &permit));
    match published {
        Ok(()) => {
            *last = Some(account);
            true
        }
        Err(e) => {
            eprintln!("monitor: could not publish its sources: {e}");
            false
        }
    }
}

/// Restart one of an author's command sources for a control client,
/// if what runs for it is still the configuration the asker was shown:
/// what to tell it, and the task stopped, to observe as it ends.
/// Only that source's task is stopped, as a config change stops it: its
/// producer is given its grace, and the replacement cannot start a run
/// until that group is gone and its lease free.
async fn restart_source(
    repo: &std::path::Path,
    hosted: &mut std::collections::BTreeMap<Producer, Hosted>,
    into: &tokio::sync::mpsc::UnboundedSender<(Stream, crate::cli::event_source::Emission)>,
    lifecycle: &LifecycleTx,
    request: &Control,
) -> Result<(String, tokio::task::JoinHandle<()>), String> {
    use clank_core::agent_config::WaitEventSource;
    let Control::Restart {
        author,
        key,
        accepted,
    } = request
    else {
        return Err("not a restart".into());
    };
    let host = hosted
        .get_mut(author)
        .ok_or_else(|| format!("the monitor runs no sources for `{author}`"))?;
    let config = host
        .spec
        .iter()
        .map(|(config, _)| config)
        .find(|config| matches!(config, WaitEventSource::Command(c) if c.source_key() == *key))
        .cloned()
        .ok_or_else(|| format!("`{author}` has no command source `{key}` running"))?;
    if crate::cli::source_status::fingerprint(&config) != *accepted {
        return Err(
            "what runs for this source changed since it was shown; reopen its page".to_string(),
        );
    }
    let old = host
        .tasks
        .remove(key)
        .ok_or_else(|| format!("`{author}`'s `{key}` has no running task"))?;
    let stopped = old.stop();
    let events_dir = author.directory(repo);
    host.start(
        author,
        &config,
        crate::cli::event_source::SourceOrigin::Config,
        &events_dir,
        &Starts::hosting(into, Some(lifecycle)),
    );
    Ok((format!("restarted `{author}`'s `{key}`"), stopped))
}

/// One author's config sources, running in the monitor.
#[derive(Default)]
pub(super) struct Hosted {
    /// The definitions what runs was started from: each task's and the
    /// timers'. A source is compared against its own entry here, so an
    /// edit restarts only the source it changed.
    pub(super) spec: Vec<(
        clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
    )>,
    /// Each source's task, by the storage key of the log it writes —
    /// the identity its durable state is filed under, so it outlives an
    /// edit to anything but the source's name.
    pub(super) tasks: std::collections::BTreeMap<String, HostedTask>,
    pub(super) timers: Option<crate::cli::timer_source::TimerSet>,
}

/// Starts a source's task under the storage key of the log it writes.
pub(super) type Spawn = dyn Fn(
    &clank_core::agent_config::WaitEventSource,
    crate::cli::event_source::SourceOrigin,
    &std::path::Path,
    crate::cli::event_source::EmissionSink,
) -> Option<crate::cli::wait::SourceTask>;

/// What a host starts its sources' tasks with.
pub(super) struct Starts<'a> {
    into: &'a tokio::sync::mpsc::UnboundedSender<(Stream, crate::cli::event_source::Emission)>,
    lifecycle: Option<&'a LifecycleTx>,
    spawn: &'a Spawn,
}

impl<'a> Starts<'a> {
    fn hosting(
        into: &'a tokio::sync::mpsc::UnboundedSender<(Stream, crate::cli::event_source::Emission)>,
        lifecycle: Option<&'a LifecycleTx>,
    ) -> Self {
        Self {
            into,
            lifecycle,
            spawn: &spawn_hosted,
        }
    }
}

/// No invocation scope: `hostable` resolves no CLI items, so every
/// source a monitor hosts is an author's config source.
fn spawn_hosted(
    source: &clank_core::agent_config::WaitEventSource,
    origin: crate::cli::event_source::SourceOrigin,
    events_dir: &std::path::Path,
    sink: crate::cli::event_source::EmissionSink,
) -> Option<crate::cli::wait::SourceTask> {
    crate::cli::wait::spawn_source(source, origin, Some(events_dir), "", sink, true)
}

/// One hosted source's task, and which instance of it this is.
pub(super) struct HostedTask {
    pub(super) instance: u64,
    pub(super) handle: tokio::task::JoinHandle<()>,
    /// Asks a command source to end within its grace. A task without
    /// one is cancelled.
    pub(super) stop: Option<tokio::sync::watch::Sender<bool>>,
}

impl HostedTask {
    /// Begin ending this task, and hand back its handle to observe the
    /// end: a command source drains its producer first, so a stop is
    /// never awaited in place.
    #[must_use = "a stopped task has not necessarily ended; observe it"]
    fn stop(self) -> tokio::task::JoinHandle<()> {
        let HostedTask { handle, stop, .. } = self;
        match stop {
            Some(stop) => {
                let _ = stop.send(true);
            }
            None => handle.abort(),
        }
        handle
    }
}

impl Hosted {
    /// Stop this author's producers and hand back their handles.
    ///
    /// Stopping only BEGINS an end; it does not establish that the
    /// task's future and the resources it owns have been dropped. A
    /// retirement that discarded these could release the lease while
    /// that was still pending (codex on d2b012d), so the handles come
    /// back and the caller observes them end.
    #[must_use = "a stopped task has not necessarily ended; observe these"]
    pub(super) fn stop(mut self) -> Vec<tokio::task::JoinHandle<()>> {
        self.timers.take();
        std::mem::take(&mut self.tasks)
            .into_values()
            .map(HostedTask::stop)
            .collect()
    }

    /// Start `source`'s task as a new instance, if it runs one.
    fn start(
        &mut self,
        author: &Producer,
        source: &clank_core::agent_config::WaitEventSource,
        origin: crate::cli::event_source::SourceOrigin,
        events_dir: &std::path::Path,
        starts: &Starts<'_>,
    ) {
        let instance = next_instance();
        let sink = host_sink(
            starts.into,
            starts.lifecycle,
            author,
            instance,
            std::slice::from_ref(source),
        );
        if let Some(task) = (starts.spawn)(source, origin, events_dir, sink) {
            self.tasks.insert(
                task.key,
                HostedTask {
                    instance,
                    handle: task.handle,
                    stop: task.stop,
                },
            );
        }
    }

    /// Cancel all production at a stand-down, once its drain has had its
    /// chance to retire every group, preserving the accepted spec for the
    /// owner's final presentation; passive future deadlines included.
    pub(super) fn stop_producing(&mut self) -> Vec<tokio::task::JoinHandle<()>> {
        self.timers.take();
        for task in self.tasks.values() {
            task.handle.abort();
        }
        std::mem::take(&mut self.tasks)
            .into_values()
            .map(|task| task.handle)
            .collect()
    }
}

/// An author's CONFIG sources — the only kind the monitor may host.
///
/// A `--event` source belongs to ONE INVOCATION and is never pooled:
/// a monitor that ran one would deliver an invocation's events to
/// whoever else happened to be that author, so it keeps its wait
/// watching for itself.
pub(super) fn hostable(
    repo: &std::path::Path,
    author: &AgentLabel,
) -> anyhow::Result<
    Vec<(
        clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
    )>,
> {
    crate::cli::wait::resolve_wait_event_sources(repo, author, &[])
}

fn owned_sources(
    repo: &std::path::Path,
    owner: &Producer,
) -> anyhow::Result<
    Vec<(
        clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
    )>,
> {
    match owner {
        Producer::Agent(author) => hostable(repo, author),
        Producer::Watch { id, .. } => {
            let config = crate::cli::watch_store::read(repo)?;
            let watch = config
                .event_watches
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("unknown watch {id}"))?;
            let sources = vec![(
                watch.source.clone(),
                crate::cli::event_source::SourceOrigin::Config,
            )];
            crate::cli::wait::validate_wait_event_sources(&sources)?;
            Ok(sources)
        }
    }
}

/// Every agent this repo's roster names.
///
/// The ROSTER and not the connected clients, which is the whole point:
/// tying a producer to whoever happens to be attached is tying it to a
/// delivery client again, and the churn that causes is what this plan
/// exists to end. A monitor's producers live as long as the monitor.
///
/// A roster that cannot be read names no one: what is notified or
/// recovered on its word waits until it can be read again.
pub(super) fn roster_authors(repo: &std::path::Path) -> Vec<AgentLabel> {
    crate::agent_store::roster_labels(repo).unwrap_or_else(|error| {
        eprintln!("monitor: the roster cannot be read: {error:#}");
        Vec::new()
    })
}

#[cfg(test)]
#[must_use = "a stopped producer may still be winding down; keep its handle"]
pub(super) fn reconcile_hosting(
    repo: &std::path::Path,
    hosted: &mut std::collections::BTreeMap<Producer, Hosted>,
    into: &tokio::sync::mpsc::UnboundedSender<(Stream, crate::cli::event_source::Emission)>,
) -> Vec<tokio::task::JoinHandle<()>> {
    reconcile_hosting_changed(repo, hosted, into, false)
}

#[cfg(test)]
#[must_use = "a stopped producer may still be winding down; keep its handle"]
fn reconcile_hosting_changed(
    repo: &std::path::Path,
    hosted: &mut std::collections::BTreeMap<Producer, Hosted>,
    into: &tokio::sync::mpsc::UnboundedSender<(Stream, crate::cli::event_source::Emission)>,
    timers_changed: bool,
) -> Vec<tokio::task::JoinHandle<()>> {
    reconcile_hosting_reporting(repo, hosted, into, timers_changed, None)
}

/// The channel a hosted source's lifecycle is reported on.
type LifecycleTx = tokio::sync::mpsc::UnboundedSender<crate::cli::event_source::LifecycleReport>;

/// What each hosted source's producer last reported, with the instance of
/// the task that reported it.
type Lifecycles =
    std::collections::BTreeMap<(Producer, String), (u64, crate::cli::event_source::Lifecycle)>;

/// A fresh instance number for tasks this monitor spawns, so a replaced
/// task's late reports are known for what they are.
fn next_instance() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Keep a producer's report if it is from the task hosted for its source
/// now; a replaced task's late word is dropped. Whether it was kept.
fn record_lifecycle(
    hosted: &std::collections::BTreeMap<Producer, Hosted>,
    lifecycles: &mut Lifecycles,
    (author, key, instance, state): crate::cli::event_source::LifecycleReport,
) -> bool {
    if !current(hosted, &author, &key, instance) {
        return false;
    }
    lifecycles.insert((author, key), (instance, state));
    true
}

/// Forget what replaced or removed tasks last said: a source's new task
/// has said nothing yet, and its old one's word is not about it.
fn forget_replaced(
    hosted: &std::collections::BTreeMap<Producer, Hosted>,
    lifecycles: &mut Lifecycles,
) {
    lifecycles.retain(|(author, key), (instance, _)| current(hosted, author, key, *instance));
}

/// Whether `instance` is the task `author` hosts for `key` now.
pub(super) fn current(
    hosted: &std::collections::BTreeMap<Producer, Hosted>,
    author: &Producer,
    key: &str,
    instance: u64,
) -> bool {
    hosted
        .get(author)
        .and_then(|h| h.tasks.get(key))
        .is_some_and(|task| task.instance == instance)
}

/// Bring hosting into line with the roster and its configs, reporting
/// each hosted source's lifecycle on `lifecycle`.
///
/// Called on every fold, because `.clank/` is a wake dir and an edited
/// `wait_events` must take effect without restarting the monitor.
///
/// Source by source: a task whose definition is unchanged is left
/// running as the same instance, a removed or changed one is stopped,
/// and only what is new or changed is started. Timers are reconciled
/// apart from the tasks, so neither ever restarts the other.
fn reconcile_hosting_reporting(
    repo: &std::path::Path,
    hosted: &mut std::collections::BTreeMap<Producer, Hosted>,
    into: &tokio::sync::mpsc::UnboundedSender<(Stream, crate::cli::event_source::Emission)>,
    timers_changed: bool,
    lifecycle: Option<&LifecycleTx>,
) -> Vec<tokio::task::JoinHandle<()>> {
    reconcile_hosting_with(
        repo,
        hosted,
        timers_changed,
        &Starts::hosting(into, lifecycle),
    )
}

fn reconcile_hosting_with(
    repo: &std::path::Path,
    hosted: &mut std::collections::BTreeMap<Producer, Hosted>,
    timers_changed: bool,
    starts: &Starts<'_>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let retry_timers = |timers: &mut crate::cli::timer_source::TimerSet| {
        if timers_changed {
            timers.inventory_changed();
        }
    };
    let mut departing = Vec::new();
    // A roster that cannot be read is not an empty one: stopping every
    // producer on it would drop what they hold until the next valid edit.
    let config = match crate::cli::watch_store::read(repo) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("monitor: cannot read source ownership; keeping hosted sources: {error:#}");
            return departing;
        }
    };
    let authors: Vec<Producer> = config
        .agents
        .keys()
        .cloned()
        .map(Producer::Agent)
        .chain(
            crate::cli::watch_store::Watches(&config)
                .hosted()
                .map(|(producer, _)| producer),
        )
        .collect();
    for author in &authors {
        // An invalid edit is a rejected replacement, not an empty config.
        // Keep the last accepted drivers and definitions until a valid edit.
        let spec = match owned_sources(repo, author) {
            Ok(spec) => spec,
            Err(error) => {
                eprintln!("monitor: rejected source config for `{author}`: {error}");
                if let Some(running) = hosted.get_mut(author)
                    && let Some(timers) = &mut running.timers
                {
                    retry_timers(timers);
                }
                continue;
            }
        };
        let host = hosted.entry(author.clone()).or_default();
        if host.spec != spec {
            // Once per change of this author's sources, not on every fold.
            for (source, _) in &spec {
                if let Some(problem) = crate::cli::event_interrupt::hint_problem(source) {
                    eprintln!("monitor: `{author}`: {problem}");
                }
            }
        }
        let events_dir = author.directory(repo);
        departing.extend(reconcile_tasks(host, author, &spec, &events_dir, starts));
        let timers = reconcile_timers(host, author, &spec, &events_dir, starts, retry_timers);
        host.spec = spec
            .into_iter()
            .filter(|(s, _)| !is_timer(s))
            .chain(timers)
            .collect();
        if host.tasks.is_empty() && host.timers.is_none() {
            hosted.remove(author);
        }
    }
    // An author off the roster keeps nothing running: a revoked agent's
    // producer is a producer nobody can ever be served from.
    let gone: Vec<Producer> = hosted
        .keys()
        .filter(|a| !authors.contains(a))
        .cloned()
        .collect();
    for author in gone {
        if let Some(running) = hosted.remove(&author) {
            departing.extend(running.stop());
        }
    }
    departing
}

fn is_timer(source: &clank_core::agent_config::WaitEventSource) -> bool {
    matches!(source, clank_core::agent_config::WaitEventSource::Timer(_))
}

/// Bring `host`'s tasks into line with `spec`, matching each by the
/// storage key it owns and replacing it only when its definition
/// changed. The stopped tasks, to observe as they end.
fn reconcile_tasks(
    host: &mut Hosted,
    author: &Producer,
    spec: &[(
        clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
    )],
    events_dir: &std::path::Path,
    starts: &Starts<'_>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let by_key = |spec: &[(
        clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
    )]| {
        spec.iter()
            .filter_map(|entry| {
                Some((
                    crate::cli::wait::task_key(&entry.0, entry.1, "")?,
                    entry.clone(),
                ))
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let wanted = by_key(spec);
    let accepted = by_key(&host.spec);
    let replaced: Vec<String> = host
        .tasks
        .keys()
        .filter(|key| match wanted.get(*key) {
            Some(definition) => accepted.get(*key) != Some(definition),
            None => true,
        })
        .cloned()
        .collect();
    let stopped = replaced
        .iter()
        .filter_map(|key| host.tasks.remove(key))
        .map(HostedTask::stop)
        .collect();
    for (key, (source, origin)) in &wanted {
        if !host.tasks.contains_key(key) {
            host.start(author, source, *origin, events_dir, starts);
        }
    }
    stopped
}

/// Bring `host`'s timers into line with `spec`, leaving an unchanged set
/// as it is and carrying each unchanged timer's deadline into a changed
/// one. The timer definitions now accepted: the old ones when the new
/// set cannot be built.
fn reconcile_timers(
    host: &mut Hosted,
    author: &Producer,
    spec: &[(
        clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
    )],
    events_dir: &std::path::Path,
    starts: &Starts<'_>,
    retry_timers: impl Fn(&mut crate::cli::timer_source::TimerSet),
) -> Vec<(
    clank_core::agent_config::WaitEventSource,
    crate::cli::event_source::SourceOrigin,
)> {
    let timers = |spec: &[(
        clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
    )]| {
        spec.iter()
            .filter(|(s, _)| is_timer(s))
            .cloned()
            .collect::<Vec<_>>()
    };
    let wanted = timers(spec);
    let accepted = timers(&host.spec);
    if wanted.is_empty() {
        host.timers = None;
        return wanted;
    }
    if wanted == accepted
        && let Some(running) = &mut host.timers
    {
        // New or replaced sets already attempted catchup; do not try
        // them twice.
        retry_timers(running);
        return wanted;
    }
    let sink = host_sink(
        starts.into,
        starts.lifecycle,
        author,
        next_instance(),
        &wanted.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>(),
    );
    // Constructors neither ingest nor spawn: a set that cannot be built
    // leaves the running one untouched.
    match crate::cli::timer_source::TimerSet::new(&wanted, events_dir, None, sink) {
        Ok(mut timers) => {
            if let Some(previous) = host.timers.take() {
                timers.keep_unchanged(previous);
            }
            timers.arm();
            host.timers = Some(timers);
            wanted
        }
        Err(error) => {
            eprintln!("monitor: timer config for `{author}`: {error}");
            if let Some(running) = &mut host.timers {
                retry_timers(running);
            }
            accepted
        }
    }
}

/// What of an emission the monitor must carry itself.
///
/// Empty for anything the log holds, because the log is what decides
/// who is owed it and when. The item itself only for a `Transient`:
/// github keeps delivering after a failed append so the agent still
/// learns about the event, and that copy is the only record there will
/// ever be.
///
/// Reads the provenance the producer SENT rather than inspecting the
/// logs. Two rounds were spent inferring it and both were wrong: a
/// durable emission is not equal to its stored row, because
/// presentation is stamped after the append, and absence proves
/// nothing anyway once a row can be compacted away (codex on 0555c8f,
/// aa3d9a5).
pub(super) fn transient_of(emission: crate::cli::event_source::Emission) -> Vec<WaitItem> {
    use crate::cli::event_source::Durability;
    match emission {
        (_, Durability::Logged | Durability::Refused) => Vec::new(),
        (item, Durability::Transient) => vec![item],
    }
}

/// THE producers for this repo: one set per author and hosted watch,
/// owned by this process rather than by whichever parked wait happened
/// to take the ingest lease — which is what stops the user's command
/// being respawned on every delivery — and what they emit that no log
/// holds.
pub(super) struct Sources {
    birth: Option<crate::proc_identity::ProcToken>,
    account: Option<crate::cli::source_status::Account>,
    /// What each hosted command source's producer last reported, published
    /// with the sources for the panels that show them.
    lifecycles: Lifecycles,
    /// Received from only.
    pub(super) lifecycles_in:
        tokio::sync::mpsc::UnboundedReceiver<crate::cli::event_source::LifecycleReport>,
    lifecycle_tx: LifecycleTx,
    hosted: std::collections::BTreeMap<Producer, Hosted>,
    /// Transients with nowhere to go yet. A genuine non-durable
    /// emission arriving before the first fold has no state to present
    /// against AND no row for a later fold to replay, so dropping it
    /// loses it outright (codex on aa3d9a5).
    pending: Vec<(Stream, WaitItem)>,
    /// A SIGNAL, never a payload. What a client is owed is decided in
    /// one place, from the durable log and that client's own `shown`
    /// set; an item carried here and sent straight on would bypass both
    /// — delivering a row a later admission replays again, or one some
    /// other client has since acked (codex on ca2ea69). Received from
    /// only, until a stand-down closes it.
    pub(super) from_sources: tokio::sync::mpsc::UnboundedReceiver<Emitted>,
    source_items: tokio::sync::mpsc::UnboundedSender<Emitted>,
    timer_backlogs: std::collections::BTreeMap<AgentLabel, crate::cli::timer_source::Backlog>,
}

/// What a hosted source emitted, and for whom.
type Emitted = (Stream, crate::cli::event_source::Emission);

/// How one pass reads what authors are owed from their timer logs:
/// through the inventory this monitor caches, or afresh, into an
/// inventory of the pass's own, which a cache that may still hold a row
/// acknowledged since its last invalidation cannot stand in for.
pub(super) struct Reading {
    cached: bool,
    fresh: std::collections::BTreeMap<AgentLabel, crate::cli::timer_source::Backlog>,
}

impl Reading {
    pub(super) fn cached() -> Self {
        Reading {
            cached: true,
            fresh: Default::default(),
        }
    }

    pub(super) fn afresh() -> Self {
        Reading {
            cached: false,
            fresh: Default::default(),
        }
    }
}

impl Sources {
    /// Host `repo`'s sources and publish what runs: with the producers a
    /// first reconcile stopped, to observe as they end.
    pub(super) fn start(repo: &std::path::Path) -> (Self, Vec<tokio::task::JoinHandle<()>>) {
        let (source_items, from_sources) = tokio::sync::mpsc::unbounded_channel::<Emitted>();
        let (lifecycle_tx, lifecycles_in) = tokio::sync::mpsc::unbounded_channel();
        let mut sources = Sources {
            birth: crate::proc_identity::token_for(std::process::id() as i32),
            account: None,
            lifecycles: Default::default(),
            lifecycles_in,
            lifecycle_tx,
            hosted: Default::default(),
            pending: Vec::new(),
            from_sources,
            source_items,
            timer_backlogs: Default::default(),
        };
        let departing = reconcile_hosting_reporting(
            repo,
            &mut sources.hosted,
            &sources.source_items,
            false,
            Some(&sources.lifecycle_tx),
        );
        sources.publish(repo);
        (sources, departing)
    }

    /// Hosting `hosted` and holding nothing, with no account to publish.
    #[cfg(test)]
    pub(super) fn hosting(hosted: std::collections::BTreeMap<Producer, Hosted>) -> Self {
        let (source_items, from_sources) = tokio::sync::mpsc::unbounded_channel::<Emitted>();
        let (lifecycle_tx, lifecycles_in) = tokio::sync::mpsc::unbounded_channel();
        Sources {
            birth: None,
            account: None,
            lifecycles: Default::default(),
            lifecycles_in,
            lifecycle_tx,
            hosted,
            pending: Vec::new(),
            from_sources,
            source_items,
            timer_backlogs: Default::default(),
        }
    }

    #[cfg(test)]
    pub(super) fn into_parts(
        self,
    ) -> (
        std::collections::BTreeMap<Producer, Hosted>,
        Vec<(Stream, WaitItem)>,
    ) {
        (self.hosted, self.pending)
    }

    #[cfg(test)]
    pub(super) fn into_hosted(self) -> std::collections::BTreeMap<Producer, Hosted> {
        self.hosted
    }

    #[cfg(test)]
    pub(super) fn host(&mut self, hosted: std::collections::BTreeMap<Producer, Hosted>) {
        self.hosted = hosted;
    }

    /// What a test's producer sends on, as a hosted one would.
    #[cfg(test)]
    pub(super) fn emitter(&self) -> tokio::sync::mpsc::UnboundedSender<Emitted> {
        self.source_items.clone()
    }

    #[cfg(test)]
    pub(super) fn pending(&self) -> &[(Stream, WaitItem)] {
        &self.pending
    }

    /// Hold `items`, owed to their streams' recipients until delivered.
    #[cfg(test)]
    pub(super) fn hold(&mut self, items: impl IntoIterator<Item = (Stream, WaitItem)>) {
        self.pending.extend(items);
    }

    #[cfg(test)]
    pub(super) fn hold_one(&mut self, item: (Stream, WaitItem)) {
        self.pending.push(item);
    }

    pub(super) fn timer_deadline(&self) -> Option<tokio::time::Instant> {
        self.hosted
            .values()
            .filter_map(|host| {
                host.timers
                    .as_ref()
                    .and_then(|timers| timers.next_deadline())
            })
            .min()
    }

    pub(super) fn timers_due(&mut self) {
        for host in self.hosted.values_mut() {
            if let Some(timers) = &mut host.timers {
                timers.due();
            }
        }
    }

    pub(super) fn invalidate_backlogs(&mut self) {
        for backlog in self.timer_backlogs.values_mut() {
            backlog.invalidate();
        }
    }

    /// Re-run each hosted timer's contended arm or acceptance.
    pub(super) fn retry_contended(&mut self) {
        for host in self.hosted.values_mut() {
            if let Some(timers) = &mut host.timers {
                timers.retry_contended();
            }
        }
    }

    /// Whether a hosted timer's step met a log another process held and
    /// has yet to run again unheld.
    pub(super) fn unavailable(&self) -> bool {
        self.hosted.values().any(|host| {
            host.timers
                .as_ref()
                .is_some_and(crate::cli::timer_source::TimerSet::awaiting_retry)
        })
    }

    /// A source produced something: held if no log holds it, and its
    /// recipient's interruption looked at by an owner. The author it is
    /// for, whose clients are then served.
    pub(super) fn emitted(
        &mut self,
        owner: bool,
        repo: &std::path::Path,
        source_owner: Stream,
        emission: crate::cli::event_source::Emission,
        owned: &mut OwnedTasks,
    ) -> Option<AgentLabel> {
        let author = source_owner.recipient(repo);
        if owner
            && let Some(author) = &author
            && roster_authors(repo).contains(author)
        {
            owned.interrupt(repo, author.clone());
        }
        if matches!(emission.0, WaitItem::TimerEvent { .. })
            && let Some(backlog) = author.as_ref().and_then(|a| self.timer_backlogs.get_mut(a))
        {
            backlog.invalidate();
        }
        self.pending.extend(
            transient_of(emission)
                .into_iter()
                .map(|item| (source_owner.clone(), item)),
        );
        author
    }

    /// Keep a producer's report if it is from the task hosted for its
    /// source now: whether it was kept.
    pub(super) fn lifecycle(&mut self, report: crate::cli::event_source::LifecycleReport) -> bool {
        record_lifecycle(&self.hosted, &mut self.lifecycles, report)
    }

    /// Bring hosting into line with the roster and its configs, the
    /// producers it stops handed to `owned` to observe as they end.
    /// Whether a changed account was published.
    pub(super) fn reconcile(
        &mut self,
        repo: &std::path::Path,
        timers_changed: bool,
        owned: &mut OwnedTasks,
    ) -> bool {
        owned.depart(reconcile_hosting_reporting(
            repo,
            &mut self.hosted,
            &self.source_items,
            timers_changed,
            Some(&self.lifecycle_tx),
        ));
        forget_replaced(&self.hosted, &mut self.lifecycles);
        self.publish(repo)
    }

    /// [`restart_source`] for a control client: what to tell it, and the
    /// task it stopped, to observe as it ends.
    pub(super) async fn restart(
        &mut self,
        repo: &std::path::Path,
        request: &Control,
    ) -> Result<(String, tokio::task::JoinHandle<()>), String> {
        let restarted = restart_source(
            repo,
            &mut self.hosted,
            &self.source_items,
            &self.lifecycle_tx,
            request,
        )
        .await;
        if restarted.is_ok()
            && let Control::Restart { author, key, .. } = request
        {
            self.lifecycles.remove(&(author.clone(), key.clone()));
        }
        restarted
    }

    /// [`publish_sources`]: whether a changed account was published.
    pub(super) fn publish(&mut self, repo: &std::path::Path) -> bool {
        match &self.birth {
            Some(birth) => publish_sources(
                repo,
                sources_account(&self.hosted, &self.lifecycles, birth.clone()),
                &mut self.account,
            ),
            None => false,
        }
    }

    /// Cancel all production for a stand-down: the producers, to observe
    /// as they end.
    pub(super) fn stop_producing(&mut self) -> Vec<tokio::task::JoinHandle<()>> {
        let mut producers = Vec::new();
        for running in self.hosted.values_mut() {
            producers.extend(running.stop_producing());
        }
        producers
    }

    /// Admit no more emissions, and hold every transient already sent.
    pub(super) fn close_emissions(&mut self) {
        self.from_sources.close();
        while let Ok((author, emission)) = self.from_sources.try_recv() {
            self.pending.extend(
                transient_of(emission)
                    .into_iter()
                    .map(|item| (author.clone(), item)),
            );
        }
    }

    pub(super) fn undelivered(&self) -> usize {
        self.pending.len()
    }

    /// `author`'s facts now, its timer logs taken as `reading` says.
    pub(super) fn gather(
        &mut self,
        repo: &std::path::Path,
        state: Option<&crate::repo_state::RepoState>,
        author: &AgentLabel,
        reading: &mut Reading,
    ) -> crate::cli::turn_custody::Gathered {
        let backlogs = match reading.cached {
            true => &mut self.timer_backlogs,
            false => &mut reading.fresh,
        };
        gather(repo, state, author, &self.hosted, backlogs, &self.pending)
    }

    /// [`owed_now`], through the cached timer logs.
    pub(super) fn owed_now(
        &mut self,
        repo: &std::path::Path,
        state: &crate::repo_state::RepoState,
        author: &AgentLabel,
    ) -> Option<Vec<WaitItem>> {
        owed_now(
            repo,
            state,
            author,
            &self.hosted,
            &mut self.timer_backlogs,
            &self.pending,
        )
    }

    /// [`admitted_items`], through the cached timer logs.
    pub(super) fn admitted(
        &mut self,
        repo: &std::path::Path,
        state: &crate::repo_state::RepoState,
        author: &AgentLabel,
        reviews: &crate::fs_plan_state_lookup::FsPlanStateLookup,
        head: Option<&clank_core::wait::HeadCommit>,
        sidecars: &crate::cli::github::log::Sidecars,
    ) -> (
        Vec<crate::cli::wait::Admitted>,
        Option<clank_core::wait::WorkStatus>,
    ) {
        admitted_items(
            repo,
            state,
            author,
            &self.hosted,
            &mut self.timer_backlogs,
            &self.pending,
            reviews,
            head,
            sidecars,
        )
    }

    /// [`owed_items`], `author`'s timer logs read for inspection.
    pub(super) fn owed_inspected(
        &self,
        repo: &std::path::Path,
        state: &crate::repo_state::RepoState,
        author: &AgentLabel,
        reviews: &crate::fs_plan_state_lookup::FsPlanStateLookup,
        head: Option<&clank_core::wait::HeadCommit>,
    ) -> Option<Vec<WaitItem>> {
        // Read afresh, never the loop's cached backlog: a recurring occurrence
        // acknowledged since its last invalidation is still in that cache.
        let backlog = crate::cli::timer_source::Backlog::new(
            crate::agent_store::agents_root(repo)
                .join(author.as_str())
                .join("events"),
            Default::default(),
        )
        .inspection();
        owed_items(
            repo,
            state,
            author,
            &self.hosted,
            &mut [(author.clone(), backlog)].into_iter().collect(),
            &self.pending,
            reviews,
            head,
        )
    }

    /// What `author`'s sources owe a client that has been shown `shown`,
    /// which is extended by what is handed it: its configured sources'
    /// unshown rows, its timers' and what is held for it.
    pub(super) fn external(
        &mut self,
        repo: &std::path::Path,
        author: &AgentLabel,
        shown: &mut std::collections::HashSet<(String, u64, String)>,
        sidecars: &crate::cli::github::log::Sidecars,
        reading: &mut Reading,
    ) -> Vec<WaitItem> {
        // Presentation uses the same accepted spec as production. A
        // rejected edit cannot hide the retained drivers' WAL rows or
        // apply unaccepted GitHub prompt changes to their delivery.
        let current;
        let spec = match self.hosted.get(&Producer::Agent(author.clone())) {
            Some(host) => &host.spec,
            None => {
                current = hostable(repo, author).unwrap_or_default();
                &current
            }
        };
        let mut external =
            crate::cli::event_source::owed_from_sources(repo, author, spec, shown, sidecars);
        let backlogs = match reading.cached {
            true => &mut self.timer_backlogs,
            false => &mut reading.fresh,
        };
        external.extend(
            backlogs
                .entry(author.clone())
                .or_insert_with(|| {
                    crate::cli::timer_source::Backlog::new(
                        crate::agent_store::agents_root(repo)
                            .join(author.as_str())
                            .join("events"),
                        Default::default(),
                    )
                })
                .owed(shown, sidecars),
        );
        // An emission no log holds exists nowhere else, so it is merged
        // into THIS presentation rather than sent after it. A client
        // returns on its first non-empty frame, so a second frame
        // carrying the transients reaches a client that has already gone
        // — and they have no row to replay (codex on 328677d). Each
        // client of this author gets them, exactly as its own local wait
        // would have.
        external.extend(
            self.pending
                .iter()
                .filter(|(whose, _)| whose.recipient(repo).as_ref() == Some(author))
                .map(|(_, item)| item.clone()),
        );
        external
    }

    /// `delivered` were woken, and so received what was held for them.
    /// What is held for an author with nobody to receive it, or whose
    /// client was retired on that very frame, keeps waiting rather than
    /// be dropped, since nothing else will ever hold it.
    pub(super) fn delivered(&mut self, repo: &std::path::Path, delivered: &[AgentLabel]) {
        self.pending.retain(|(owner, _)| {
            owner
                .recipient(repo)
                .is_none_or(|author| !delivered.contains(&author))
        });
    }

    /// A delivery to `author` discharged the transient items `commit`
    /// presented; anything else stays held, owed to the next presentation.
    pub(super) fn discharge(
        &mut self,
        repo: &std::path::Path,
        author: &AgentLabel,
        commit: &crate::cli::turn_custody::Commit,
    ) {
        self.pending.retain(|(whose, item)| {
            let ids = identities(std::slice::from_ref(item));
            let discharged =
                !ids.is_empty() && ids.iter().all(|id| commit.presented().contains(id));
            whose.recipient(repo).as_ref() != Some(author) || !discharged
        });
    }
}

#[cfg(test)]
#[path = "hosted_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "producer_identity_tests.rs"]
mod producer_identity_tests;
