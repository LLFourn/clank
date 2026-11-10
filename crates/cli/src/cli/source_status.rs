//! The event sources agents have configured, as the status panel lists
//! them: who owns each, the log it writes, and the icon its events carry.

use clank_core::agent_config::WaitEventSource;
use clank_core::ids::AgentLabel;

use crate::cli::github::timeline::{Backend, MergedEvent};

/// One configured source. It is identified by its owner and the key of
/// the log its events land in, never by its display name: two agents may
/// each have a source of the same name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfiguredSource {
    pub(crate) agent: AgentLabel,
    pub(crate) key: String,
    pub(crate) backend: Backend,
    pub(crate) name: String,
    pub(crate) glyph: String,
    pub(crate) config: WaitEventSource,
    /// What a github watch subscribes to; a command source has none.
    pub(crate) subscription: Option<String>,
    /// How a github watch is configured to poll.
    pub(crate) polling: Option<String>,
    /// A timer's state, from its durable evidence; set by [`configured`].
    pub(crate) timer: Option<crate::cli::timer_source::TimerState>,
}

impl ConfiguredSource {
    pub(crate) fn of(agent: &AgentLabel, config: &WaitEventSource) -> Self {
        let (backend, key, name, glyph) = match config {
            WaitEventSource::Command(c) => (
                Backend::Command,
                c.source_key(),
                c.name.clone(),
                c.glyph.as_deref(),
            ),
            WaitEventSource::Timer(t) => (
                Backend::Timer,
                crate::cli::timer_source::config_key(t),
                t.name.clone(),
                None,
            ),
            WaitEventSource::Github(g) => (
                Backend::Github,
                crate::cli::github::supervisor::source_key(g),
                g.repo.clone(),
                None,
            ),
        };
        let (subscription, polling) = match config {
            WaitEventSource::Github(g) => (
                Some(crate::cli::github::selection::Selection::of(g).describe()),
                Some(crate::cli::github::configured_polling(g)),
            ),
            _ => (None, None),
        };
        Self {
            agent: agent.clone(),
            key,
            backend,
            name,
            glyph: crate::cli::log::badge_of(glyph, backend).to_string(),
            config: config.clone(),
            subscription,
            polling,
            timer: None,
        }
    }

    /// Whether `event` came from this source: one of its copies is in this
    /// source's log, or it is a reply sent from an event that was.
    pub(crate) fn produced(&self, event: &MergedEvent) -> bool {
        let ours = |agent: &str, source: &str| agent == self.agent.as_str() && source == self.key;
        event
            .members
            .iter()
            .any(|m| ours(&m.key.agent, &m.key.source))
            || event
                .outgoing
                .iter()
                .any(|o| ours(&o.original.agent, &o.original.source))
    }
}

/// Every source the roster's agents configure, by name and then agent:
/// the external ones, and each timer that has not yet fired, with its
/// state. Messages are always on, so they are not sources here; nor is a
/// config an agent off the roster left on disk. A roster agent's config
/// that cannot be read is an error, not an agent with no sources.
pub(crate) fn configured(repo: &std::path::Path) -> anyhow::Result<Vec<ConfiguredSource>> {
    let mut configs = Vec::new();
    for label in crate::agent_store::roster_labels(repo)? {
        if let Some(cfg) = crate::agent_store::load_agent_config(repo, &label)? {
            configs.push((label, cfg));
        }
    }
    let mut sources: Vec<ConfiguredSource> = configs
        .iter()
        .flat_map(|(label, cfg)| {
            cfg.wait_events
                .iter()
                .filter_map(|s| {
                    let mut source = ConfiguredSource::of(label, s);
                    if let WaitEventSource::Timer(t) = s {
                        let events = crate::agent_store::agents_root(repo)
                            .join(label.as_str())
                            .join("events");
                        let state = crate::cli::timer_source::inspect(&events, t);
                        if state == crate::cli::timer_source::TimerState::Fired {
                            return None;
                        }
                        source.timer = Some(state);
                    }
                    Some(source)
                })
                .collect::<Vec<_>>()
        })
        .collect();
    sources.sort_by(|a, b| {
        (&a.name, a.agent.as_str(), &a.key).cmp(&(&b.name, b.agent.as_str(), &b.key))
    });
    Ok(sources)
}

/// What a source's configuration is, as one comparable value: what the
/// monitor accepted, and what a restart asks to be the one it restarts.
pub(crate) fn fingerprint(config: &WaitEventSource) -> String {
    blake3::hash(&serde_json::to_vec(config).unwrap_or_default())
        .to_hex()
        .to_string()
}

/// What the monitor hosts for one source: the configuration it accepted,
/// and its producer's lifecycle when it has one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Hosted {
    pub(crate) agent: String,
    pub(crate) key: String,
    pub(crate) accepted: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) lifecycle: Option<crate::cli::event_source::Lifecycle>,
    /// The tag of a lifecycle this build cannot read: a monitor of
    /// another clank version wrote it. Never written.
    #[serde(skip)]
    pub(crate) foreign: Option<String>,
}

/// A lifecycle this build does not know costs that one source its
/// state, never the whole account: the reader and the monitor are
/// routinely different builds across an install.
impl<'de> serde::Deserialize<'de> for Hosted {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Raw {
            agent: String,
            key: String,
            accepted: String,
            #[serde(default)]
            lifecycle: Option<serde_json::Value>,
        }
        let raw = Raw::deserialize(d)?;
        let (lifecycle, foreign) = match raw.lifecycle {
            None => (None, None),
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(l) => (Some(l), None),
                Err(_) => (
                    None,
                    Some(
                        v.get("state")
                            .and_then(|s| s.as_str())
                            .unwrap_or("unknown")
                            .to_string(),
                    ),
                ),
            },
        };
        Ok(Self {
            agent: raw.agent,
            key: raw.key,
            accepted: raw.accepted,
            lifecycle,
            foreign,
        })
    }
}

/// The monitor's account of its sources, stamped with the
/// monitor that wrote it: a file outliving its writer says nothing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Account {
    pub(crate) monitor: i32,
    pub(crate) birth: crate::proc_identity::ProcToken,
    pub(crate) sources: Vec<Hosted>,
}

/// Outside `.clank/`, beside the monitor's descriptor: a lifecycle change
/// is no repo write, so it wakes no gate, wait or monitor.
pub(crate) fn path(repo: &std::path::Path) -> std::path::PathBuf {
    crate::cli::monitor::runtime_path(repo, "sources.json")
}

fn path_in(namespace: &std::path::Path, repo: &std::path::Path) -> std::path::PathBuf {
    crate::cli::monitor::runtime_path_in(namespace, repo, "sources.json")
}

/// Replace the published account whole.
pub(crate) fn publish(
    repo: &std::path::Path,
    account: &Account,
    _permit: &crate::upgrade::Permit,
) -> std::io::Result<()> {
    let path = path(repo);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec(account).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(tmp, path)
}

/// What is known of the sources' live state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Live {
    /// The running monitor's own account, and for each source it only
    /// follows, the producer group its record proves is running, by
    /// (agent, key).
    Monitor(Account, Producers),
    /// No monitor runs for this repo, or the account is not the running
    /// one's: nothing is known, which is not the same as stopped.
    Unavailable,
    /// A monitor is published for this repo and its account is there, in
    /// a shape this build cannot read: another clank build wrote it. Not
    /// the same as no monitor.
    Unreadable,
}

/// Read the account, trusting it only while the monitor that wrote it is
/// the one published for this repo and still the process it was,
/// whatever clank version either of them is. Never starts a monitor.
pub(crate) fn live(repo: &std::path::Path) -> Live {
    live_in(
        &crate::cli::monitor::runtime_dir(rustix::process::getuid().as_raw()),
        repo,
    )
}

/// Verified producers of followed sources, by (agent, key).
pub(crate) type Producers = std::collections::BTreeMap<(String, String), i32>;

fn live_in(namespace: &std::path::Path, repo: &std::path::Path) -> Live {
    // The account proves itself by its writer's birth; the descriptor is
    // only asked which pid serves this repo. Requiring a descriptor of
    // this build's own protocol hid a running monitor from every reader
    // installed before or after it.
    let published = match crate::cli::monitor::published_in(
        namespace,
        repo,
        rustix::process::getuid().as_raw(),
    ) {
        crate::cli::monitor::Published::Ours(descriptor) => descriptor.pid,
        crate::cli::monitor::Published::Other { pid: Some(pid), .. } => pid,
        _ => return Live::Unavailable,
    };
    let Ok(raw) = std::fs::read(path_in(namespace, repo)) else {
        return Live::Unavailable;
    };
    match serde_json::from_slice::<Account>(&raw) {
        Ok(account)
            if account.monitor == published && account.birth.still_holds(account.monitor) =>
        {
            let producers = followed_producers(repo, &account);
            Live::Monitor(account, producers)
        }
        Ok(_) => Live::Unavailable,
        Err(_) => Live::Unreadable,
    }
}

/// Another host holds a followed source's lease, so its producer is
/// whatever that source's own record proves, in its author's events.
fn followed_producers(repo: &std::path::Path, account: &Account) -> Producers {
    let events = |agent: &str| {
        crate::agent_store::agents_root(repo)
            .join(agent)
            .join("events")
    };
    account
        .sources
        .iter()
        .filter(|h| h.lifecycle == Some(crate::cli::event_source::Lifecycle::Following))
        .filter_map(|h| {
            let pgid = crate::cli::command_source::verified_producer(&events(&h.agent), &h.key)?;
            Some(((h.agent.clone(), h.key.clone()), pgid))
        })
        .collect()
}

impl Live {
    /// The verified producer of `source`, when the monitor only follows
    /// it.
    pub(crate) fn producer(&self, source: &ConfiguredSource) -> Option<i32> {
        match self {
            Live::Monitor(_, producers) => producers
                .get(&(source.agent.to_string(), source.key.clone()))
                .copied(),
            Live::Unavailable | Live::Unreadable => None,
        }
    }

    /// What the monitor hosts for `source`, if it hosts it.
    pub(crate) fn of(&self, source: &ConfiguredSource) -> Option<&Hosted> {
        match self {
            Live::Monitor(p, _) => p
                .sources
                .iter()
                .find(|h| h.agent == source.agent.as_str() && h.key == source.key),
            Live::Unavailable | Live::Unreadable => None,
        }
    }
}

/// Whether a source is making the progress its kind owes. Judged on
/// whether it CHECKED, never on whether anything arrived: a quiet repo,
/// a future timer and a silent command are all healthy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Health {
    Healthy,
    Failing {
        why: String,
        /// When it began failing, when that is known (Unix seconds).
        since: Option<u64>,
    },
}

impl Health {
    pub(crate) fn failing(&self) -> bool {
        matches!(self, Health::Failing { .. })
    }
}

/// The SGR a failing source's state is drawn in: bright red, so a source
/// that has stopped checking cannot pass for a quiet one.
pub(crate) const FAILING_SGR: &str = "1;91";

/// Each configured source's line for `clank status`: its state, bright
/// red when [`source_health`] says it is failing and `paint` allows.
pub(crate) fn status_lines(
    sources: &[ConfiguredSource],
    live: &Live,
    now: u64,
    paint: bool,
) -> Vec<String> {
    sources
        .iter()
        .map(|source| {
            let state = match &source.timer {
                Some(_) => timer_text(source, now),
                None => state_text(source, live, now),
            };
            let state = match paint && source_health(source, live.of(source), now).failing() {
                true => format!("\x1b[{FAILING_SGR}m{state}\x1b[0m"),
                false => state,
            };
            format!(
                "source: {} {} @{} → {state}",
                source.glyph, source.name, source.agent
            )
        })
        .collect()
}

/// A timer's state in words at `now`: when it fires, or since when it
/// has been owed, or why it has no time.
pub(crate) fn timer_text(source: &ConfiguredSource, now: u64) -> String {
    use crate::cli::timer_source::TimerState;
    let now = now as i64;
    let recurring = matches!(
        &source.config,
        WaitEventSource::Timer(t) if t.repeat_every.is_some()
    );
    match &source.timer {
        Some(TimerState::Due(at)) if *at as i64 > now => {
            format!("in {}", crate::age::short(*at as i64 - now))
        }
        Some(TimerState::Due(at)) => {
            let since = crate::age::short(now - *at as i64);
            match recurring {
                true => format!("catch-up owed since {since}"),
                false => format!("owed since {since}"),
            }
        }
        Some(TimerState::AwaitingAck) => "awaiting ack".to_string(),
        Some(TimerState::Unknown(why)) => format!("unknown: {why}"),
        Some(TimerState::Fired) | None => String::new(),
    }
}

/// How long past its due time a timer may go undelivered before it is
/// failing: long enough for a monitor restart, short enough to notice.
pub(crate) const TIMER_GRACE: u64 = 120;

/// The one health judgement the TUI's source rows and `clank status`
/// share, from the monitor's record of `source` and `now` alone.
pub(crate) fn source_health(
    source: &ConfiguredSource,
    record: Option<&Hosted>,
    now: u64,
) -> Health {
    use crate::cli::event_source::Lifecycle;
    use crate::cli::timer_source::TimerState;
    if source.backend == Backend::Timer {
        return match &source.timer {
            Some(TimerState::Due(at)) if now > at + TIMER_GRACE => Health::Failing {
                why: format!(
                    "due {} ago and not delivered",
                    crate::age::short(now.saturating_sub(*at) as i64)
                ),
                since: Some(at + TIMER_GRACE),
            },
            Some(TimerState::Unknown(why)) => Health::Failing {
                why: format!("its state cannot be read: {why}"),
                since: None,
            },
            _ => Health::Healthy,
        };
    }
    match record.and_then(|h| h.lifecycle.as_ref()) {
        Some(Lifecycle::Polling(r)) => r.health(now),
        Some(Lifecycle::Stopped { why }) => Health::Failing {
            why: format!("stopped: {why}"),
            since: None,
        },
        Some(Lifecycle::BackingOff {
            failures,
            exit,
            error,
            ..
        }) if *failures >= crate::cli::command_source::FAILURES_BEFORE_SURFACING => {
            Health::Failing {
                why: format!("{} · {failures} restarts in a row", ended(*exit, error)),
                since: None,
            }
        }
        _ => Health::Healthy,
    }
}

fn ended(exit: Option<i32>, error: &Option<String>) -> String {
    match (exit, error) {
        (_, Some(error)) => format!("did not start: {error}"),
        (Some(code), None) => format!("exited {code}"),
        (None, None) => "ended".to_string(),
    }
}

/// What every source reads as while its monitor's account is one this
/// build cannot read.
pub(crate) const ACCOUNT_UNREADABLE: &str =
    "unknown: the monitor's account is from another clank build";

/// What the monitor says `source` is doing at `now`, in a few words.
/// Without a running monitor nothing is known, which is said, not taken
/// for stopped.
pub(crate) fn state_text(source: &ConfiguredSource, live: &Live, now: u64) -> String {
    use crate::cli::event_source::Lifecycle;
    let hosted = match live {
        Live::Unavailable => return "monitor not running".to_string(),
        Live::Unreadable => return ACCOUNT_UNREADABLE.to_string(),
        Live::Monitor(..) => match live.of(source) {
            Some(hosted) => hosted,
            None => return "not hosted".to_string(),
        },
    };
    let state = match (&hosted.lifecycle, &hosted.foreign, source.backend) {
        (None, Some(tag), _) => format!("{tag} (reported by another clank version)"),
        (None, None, Backend::Command) => "starting".to_string(),
        (None, None, Backend::Github) => source.polling.clone().unwrap_or_default(),
        (None, None, _) => "hosted".to_string(),
        (Some(Lifecycle::Following), _, _) => match live.producer(source) {
            Some(pid) => format!("held elsewhere · producer pid {pid}"),
            None => "held elsewhere · producer unknown".to_string(),
        },
        (Some(Lifecycle::Settling { why }), _, _) => format!("settling: {why}"),
        (Some(Lifecycle::Stopped { why }), _, _) => format!("stopped: {why}"),
        (Some(Lifecycle::Polling(r)), _, _) => polling_text(r, now),
        (Some(Lifecycle::Running { pid, since }), _, _) => format!(
            "running · pid {pid} · {}",
            crate::age::short(now as i64 - *since as i64)
        ),
        (
            Some(Lifecycle::BackingOff {
                until,
                failures,
                exit,
                error,
            }),
            _,
            _,
        ) => format!(
            "{} · restarting in {}s · {failures} in a row",
            ended(*exit, error),
            until.saturating_sub(now)
        ),
    };
    if hosted.accepted != fingerprint(&source.config) {
        format!("{state} · an edit was rejected; runs the earlier config")
    } else {
        state
    }
}

fn polling_text(r: &crate::cli::github::progress::PollRecord, now: u64) -> String {
    let short = |secs: u64| crate::age::short(secs as i64);
    // Coarse, because the panel repaints on a minute's idle cadence and
    // a seconds count would sit frozen between paints.
    let checked = match r
        .checked
        .map(|at| crate::age::coarse(now.saturating_sub(at) as i64))
    {
        Some(age) if age == "now" => "checked just now".to_string(),
        Some(age) => format!("checked {age} ago"),
        None => "no poll completed yet".to_string(),
    };
    if !r.errors.is_empty() {
        let failures: Vec<String> = r
            .errors
            .iter()
            .map(|e| match e.retry_at.checked_sub(now).filter(|s| *s > 0) {
                Some(left) => format!("{} · retrying in {}", e.why, short(left)),
                None => format!("{} · retrying now", e.why),
            })
            .collect();
        return format!("{} · {checked}", failures.join("; "));
    }
    match r.health(now) {
        Health::Failing { why, .. } => format!("{why} · {checked}"),
        Health::Healthy => format!("polling every {} · {checked}", short(r.every)),
    }
}

/// [`state_text`] in a word, for counting.
pub(crate) fn state_word(source: &ConfiguredSource, live: &Live, now: u64) -> &'static str {
    use crate::cli::event_source::Lifecycle;
    match live {
        Live::Monitor(..) => {}
        Live::Unavailable => return "monitor not running",
        Live::Unreadable => return ACCOUNT_UNREADABLE,
    }
    let Some(hosted) = live.of(source) else {
        return "not hosted";
    };
    match (&hosted.lifecycle, &hosted.foreign) {
        (None, Some(_)) => "unknown",
        (None, None) => "hosted",
        (Some(Lifecycle::Running { .. }), _) => "running",
        (Some(Lifecycle::Following), _) => "held elsewhere",
        (Some(Lifecycle::Settling { .. }), _) => "settling",
        (Some(Lifecycle::Stopped { .. }), _) => "stopped",
        (Some(Lifecycle::BackingOff { .. }), _) => "restarting",
        (Some(Lifecycle::Polling(r)), _) => match r.health(now) {
            Health::Healthy => "polling",
            Health::Failing { .. } => "failing",
        },
    }
}

/// The positions in `events` of what `source` produced, in their order.
pub(crate) fn events_of(source: &ConfiguredSource, events: &[MergedEvent]) -> Vec<usize> {
    events
        .iter()
        .enumerate()
        .filter(|(_, e)| source.produced(e))
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::github::log::Transport;
    use crate::cli::github::timeline::{MemberKey, MemberRef, OutgoingReply};
    use clank_core::agent_config::CommandSource;

    fn command(name: &str) -> WaitEventSource {
        WaitEventSource::Command(CommandSource {
            interrupt: false,
            interrupt_message: None,
            name: name.into(),
            command: vec!["producer".into()],
            reply_command: None,
            receipt_command: None,
            glyph: Some("🛰".into()),
            stop_grace: None,
        })
    }

    fn key(agent: &str, source: &str) -> MemberKey {
        MemberKey {
            agent: agent.into(),
            source: source.into(),
            seq: 1,
            ident: "i".into(),
        }
    }

    fn event(members: Vec<MemberKey>, outgoing: Vec<MemberKey>) -> MergedEvent {
        MergedEvent {
            replies: Vec::new(),
            reply_history_pruned: false,
            outgoing: outgoing
                .into_iter()
                .map(|original| OutgoingReply {
                    operation_id: "op".into(),
                    delivery_id: "op".into(),
                    from: original.agent.clone(),
                    from_repo: "/r".into(),
                    destination: None,
                    original,
                    original_handle: "h".into(),
                    acknowledge: false,
                })
                .collect(),
            recurrence: None,
            backend: Backend::Command,
            glyph: None,
            at: 1,
            source: "questions".into(),
            kind: "k".into(),
            content: None,
            body: None,
            failure: false,
            detail: None,
            number: None,
            actor: None,
            url: None,
            seen_by: Vec::new(),
            sender: None,
            members: members
                .into_iter()
                .map(|key| MemberRef {
                    key,
                    acked: false,
                    woken_at: None,
                    transport: Transport::Command,
                    content: None,
                })
                .collect(),
            unhandled: true,
            baseline: false,
        }
    }

    /// Two agents with a source of one name are two sources: each sees
    /// only what its own log holds, and the replies sent from it.
    #[test]
    fn a_source_is_its_owner_and_its_log_not_its_name() {
        let alpha =
            ConfiguredSource::of(&AgentLabel::parse("alpha").unwrap(), &command("questions"));
        let beta = ConfiguredSource::of(&AgentLabel::parse("beta").unwrap(), &command("questions"));
        assert_eq!(alpha.key, beta.key, "the same name, the same key");
        assert_eq!(alpha.glyph, "🛰");
        let events = [
            event(vec![key("alpha", &alpha.key)], vec![]),
            event(vec![key("beta", &beta.key)], vec![]),
            event(vec![], vec![key("alpha", &alpha.key)]),
            event(vec![key("alpha", "another-log")], vec![]),
        ];
        assert_eq!(events_of(&alpha, &events), [0, 2]);
        assert_eq!(events_of(&beta, &events), [1]);
    }

    fn account(monitor: i32) -> Account {
        Account {
            monitor,
            birth: crate::proc_identity::token_for(std::process::id() as i32).unwrap(),
            sources: vec![Hosted {
                agent: "alpha".into(),
                key: "command-questions".into(),
                accepted: "f".into(),
                lifecycle: Some(crate::cli::event_source::Lifecycle::Running { pid: 7, since: 1 }),
                foreign: None,
            }],
        }
    }

    /// The account is believed only while the monitor published for the
    /// repo is the very process that wrote it: no monitor, a monitor
    /// that is not the writer, a writer whose pid was reused, or no
    /// account at all say nothing is known.
    #[test]
    fn the_account_is_the_running_monitors_or_nothing() {
        let me = rustix::process::getuid().as_raw();
        let ns = tempfile::tempdir().unwrap();
        let namespace = ns.path().join("run");
        let repo = tempfile::tempdir().unwrap();
        assert_eq!(
            live_in(&namespace, repo.path()),
            Live::Unavailable,
            "no monitor"
        );
        let ours = std::process::id() as i32;
        crate::cli::monitor::publish_in(
            repo.path(),
            &namespace,
            me,
            &crate::cli::monitor::Descriptor {
                proto: crate::cli::monitor::PROTO,
                pid: ours,
                repo: repo.path().display().to_string(),
                socket: "/nowhere.sock".into(),
                build: None,
            },
        )
        .unwrap();
        assert_eq!(
            live_in(&namespace, repo.path()),
            Live::Unavailable,
            "no account"
        );
        let write = |a: &Account| {
            std::fs::write(
                path_in(&namespace, repo.path()),
                serde_json::to_vec(a).unwrap(),
            )
            .unwrap()
        };
        write(&account(ours));
        assert_eq!(
            live_in(&namespace, repo.path()),
            Live::Monitor(account(ours), Default::default())
        );
        write(&account(ours + 1));
        assert_eq!(
            live_in(&namespace, repo.path()),
            Live::Unavailable,
            "another writer"
        );
        let mut reused = account(ours);
        reused.birth = crate::proc_identity::ProcToken::MacStart { sec: 1, usec: 1 };
        write(&reused);
        assert_eq!(
            live_in(&namespace, repo.path()),
            Live::Unavailable,
            "a reused pid"
        );
    }

    /// The account monitor 6316 published on 9 Nov while the TUI said
    /// "monitor not running": a reader of the protocol before it, and a
    /// lifecycle (`degraded`) this build no longer has. Both are the
    /// ordinary skew across an install, so the account still shows the
    /// running monitor's sources.
    #[test]
    fn a_monitor_of_another_version_still_shows_its_sources() {
        let me = rustix::process::getuid().as_raw();
        let ns = tempfile::tempdir().unwrap();
        let namespace = ns.path().join("run");
        let repo = tempfile::tempdir().unwrap();
        let ours = std::process::id() as i32;
        crate::cli::monitor::publish_in(
            repo.path(),
            &namespace,
            me,
            &crate::cli::monitor::Descriptor {
                proto: crate::cli::monitor::PROTO,
                pid: ours,
                repo: repo.path().display().to_string(),
                socket: "/nowhere.sock".into(),
                build: None,
            },
        )
        .unwrap();
        let other_proto = crate::cli::monitor::PROTO + 1;
        std::fs::write(
            crate::cli::monitor::runtime_path_in(&namespace, repo.path(), "json"),
            format!(
                r#"{{"proto":{other_proto},"pid":{ours},"repo":"{}","socket":"/tmp/clank-socks/{ours}.sock"}}"#,
                repo.path().display()
            ),
        )
        .unwrap();
        let birth = serde_json::to_string(&crate::proc_identity::token_for(ours).unwrap()).unwrap();
        std::fs::write(
            path_in(&namespace, repo.path()),
            format!(
                r#"{{"monitor":{ours},"birth":{birth},"sources":[{{"agent":"claude","key":"github-LLFourn-clank-3718fe74","accepted":"75796871d350697a14ea6d6492d77ab28003df5870d0f2b80bf6eb44daa8ffb9","lifecycle":{{"state":"degraded","why":"another forwarder holds the webhook slot for LLFourn/clank (another clank monitor, another machine, or a `gh webhook forward` of your own); stop it to free the slot","polling":false}}}}]}}"#
            ),
        )
        .unwrap();
        let live = live_in(&namespace, repo.path());
        let Live::Monitor(account, _) = &live else {
            panic!("a running monitor's account is believed: {live:?}");
        };
        assert_eq!(account.sources[0].lifecycle, None);
        assert_eq!(account.sources[0].foreign.as_deref(), Some("degraded"));
        let source = ConfiguredSource::of(
            &AgentLabel::parse("claude").unwrap(),
            &serde_json::from_str(
                r#"{"kind":"github","repo":"LLFourn/clank","events":["ci_passed"]}"#,
            )
            .unwrap(),
        );
        let mut source = source;
        source.key = "github-LLFourn-clank-3718fe74".into();
        let text = state_text(&source, &live, 0);
        assert!(
            text.starts_with("degraded (reported by another clank version)"),
            "{text}"
        );
    }

    /// "monitor not running", for a running monitor, was the bug. An
    /// account a published monitor wrote in a shape this build cannot
    /// read is said to be that, in every wording a source is shown in;
    /// only a repo with no monitor published reads as having none.
    #[test]
    fn an_account_this_build_cannot_read_is_never_a_monitor_not_running() {
        let me = rustix::process::getuid().as_raw();
        let ns = tempfile::tempdir().unwrap();
        let namespace = ns.path().join("run");
        let repo = tempfile::tempdir().unwrap();
        let ours = std::process::id() as i32;
        let source = ConfiguredSource::of(
            &AgentLabel::parse("claude").unwrap(),
            &serde_json::from_str(
                r#"{"kind":"github","repo":"LLFourn/clank","events":["ci_passed"]}"#,
            )
            .unwrap(),
        );
        std::fs::create_dir_all(&namespace).unwrap();
        let newer = r#"{"monitor":{"pid":1,"build":"ff"},"hosted":{"claude":[]}}"#;
        std::fs::write(path_in(&namespace, repo.path()), newer).unwrap();
        assert_eq!(
            live_in(&namespace, repo.path()),
            Live::Unavailable,
            "nothing is published, so nothing vouches for the file"
        );

        crate::cli::monitor::publish_in(
            repo.path(),
            &namespace,
            me,
            &crate::cli::monitor::Descriptor {
                proto: crate::cli::monitor::PROTO,
                pid: ours,
                repo: repo.path().display().to_string(),
                socket: "/nowhere.sock".into(),
                build: None,
            },
        )
        .unwrap();
        let live = live_in(&namespace, repo.path());
        assert_eq!(live, Live::Unreadable);
        for said in [
            state_text(&source, &live, 0),
            state_word(&source, &live, 0).to_string(),
        ] {
            assert_eq!(said, ACCOUNT_UNREADABLE);
            assert!(!said.contains("not running"), "{said}");
        }
        assert_eq!(live.of(&source), None);
    }

    fn hosted(source: &ConfiguredSource, lifecycle: crate::cli::event_source::Lifecycle) -> Hosted {
        Hosted {
            agent: source.agent.to_string(),
            key: source.key.clone(),
            accepted: fingerprint(&source.config),
            lifecycle: Some(lifecycle),
            foreign: None,
        }
    }

    /// Each kind is judged on whether it checked: a silent running
    /// command is healthy; one that has stopped, or keeps restarting
    /// within the restart policy, is failing.
    #[test]
    fn a_command_is_failing_only_when_its_producer_is_not_running() {
        use crate::cli::event_source::Lifecycle;
        let s = ConfiguredSource::of(&AgentLabel::parse("alpha").unwrap(), &command("voice"));
        let health = |l| source_health(&s, Some(&hosted(&s, l)), 1_000_000);
        assert_eq!(
            health(Lifecycle::Running { pid: 9, since: 0 }),
            Health::Healthy
        );
        assert_eq!(health(Lifecycle::Following), Health::Healthy);
        let backing = |failures| Lifecycle::BackingOff {
            until: 0,
            failures,
            exit: Some(1),
            error: None,
        };
        let surfacing = crate::cli::command_source::FAILURES_BEFORE_SURFACING;
        assert_eq!(
            health(backing(surfacing - 1)),
            Health::Healthy,
            "restarting"
        );
        assert!(health(backing(surfacing)).failing(), "repeated restarts");
        assert!(
            health(Lifecycle::Stopped {
                why: "its command is empty".into()
            })
            .failing()
        );
        assert_eq!(source_health(&s, None, 0), Health::Healthy, "no word yet");
    }

    /// A timer before its fire time is healthy however far off it is;
    /// a due fire undelivered past the grace is failing.
    #[test]
    fn a_timer_fails_only_once_a_due_fire_outlives_the_grace() {
        use crate::cli::timer_source::TimerState;
        let mut s = ConfiguredSource::of(
            &AgentLabel::parse("alpha").unwrap(),
            &timer("review", "2026-01-01T08:00:00Z", None),
        );
        let at = 1_000_000;
        s.timer = Some(TimerState::Due(at));
        assert_eq!(source_health(&s, None, at - 86_400), Health::Healthy);
        assert_eq!(source_health(&s, None, at + TIMER_GRACE), Health::Healthy);
        assert_eq!(
            source_health(&s, None, at + TIMER_GRACE + 1),
            Health::Failing {
                why: "due 2m ago and not delivered".into(),
                since: Some(at + TIMER_GRACE)
            }
        );
        s.timer = Some(TimerState::AwaitingAck);
        assert_eq!(
            source_health(&s, None, at + 86_400),
            Health::Healthy,
            "delivered"
        );
    }

    /// A watch that only ever hears 304s is checking, so healthy; the
    /// health the panel and `clank status` show is the record's own.
    #[test]
    fn a_watch_answered_only_by_304s_is_healthy() {
        use crate::cli::event_source::Lifecycle;
        use crate::cli::github::progress::PollRecord;
        let s = ConfiguredSource::of(
            &AgentLabel::parse("alpha").unwrap(),
            &serde_json::from_str(r#"{"kind":"github","repo":"o/r","events":["pr_opened"]}"#)
                .unwrap(),
        );
        let record = |checked| {
            hosted(
                &s,
                Lifecycle::Polling(PollRecord {
                    since: 0,
                    every: 60,
                    backoff: 0,
                    checked: Some(checked),
                    errors: Vec::new(),
                }),
            )
        };
        assert_eq!(
            source_health(&s, Some(&record(10_000)), 10_150),
            Health::Healthy
        );
        assert!(source_health(&s, Some(&record(10_000)), 10_181).failing());
    }

    fn timer(name: &str, due_at: &str, every: Option<&str>) -> WaitEventSource {
        WaitEventSource::Timer(clank_core::agent_config::TimerSource {
            interrupt: false,
            interrupt_message: None,
            name: name.into(),
            repeat_every: every.map(String::from),
            due_at: due_at.into(),
            content: Some("follow up".into()),
        })
    }

    /// The roster's sources, by name and then agent; an agent off the
    /// roster runs nothing, so it lists nothing, and with no roster
    /// nothing is listed.
    #[test]
    fn configured_sources_are_the_rosters_external_ones() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"claude":{"tool":"claude","role":"master"},"codex":{"tool":"codex","role":"commit"}}}"#,
        )
        .unwrap();
        for (agent, sources) in [
            ("codex", vec![command("questions"), command("answers")]),
            ("claude", vec![command("questions")]),
            ("grok", vec![command("voice")]),
        ] {
            crate::agent_store::update_agent_config(
                repo.path(),
                &AgentLabel::parse(agent).unwrap(),
                |cfg| cfg.wait_events = sources,
            )
            .unwrap();
        }
        let listed: Vec<(String, String)> = configured(repo.path())
            .unwrap()
            .into_iter()
            .map(|s| (s.name, s.agent.to_string()))
            .collect();
        assert_eq!(
            listed,
            [
                ("answers".to_string(), "codex".to_string()),
                ("questions".into(), "claude".into()),
                ("questions".into(), "codex".into()),
            ]
        );
        assert!(configured(&repo.path().join("missing")).unwrap().is_empty());
    }

    /// life's shape: recurring reviews and future reminders are listed,
    /// with their state, under the scheduler's own storage keys; a past
    /// reminder is listed while it is owed and gone once the scheduler
    /// has accepted it.
    #[tokio::test]
    async fn a_timer_is_listed_until_its_occurrence_is_accepted() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"assistent":{"tool":"codex","role":"master"}}}"#,
        )
        .unwrap();
        let assistent = AgentLabel::parse("assistent").unwrap();
        let timers = vec![
            timer("btc-eth-daily-review", "2026-01-01T08:00:00Z", Some("24h")),
            timer("daily-inbox-review", "2026-01-01T09:00:00Z", Some("24h")),
            timer("rama-follow-up", "2999-01-01T00:00:00Z", None),
            timer("virus-meeting", "2999-02-01T00:00:00Z", None),
            timer("tax-bas", "2026-01-02T00:00:00Z", None),
            timer("garbled", "not a time", None),
        ];
        crate::agent_store::update_agent_config(repo.path(), &assistent, |cfg| {
            cfg.wait_events = timers.clone()
        })
        .unwrap();
        let names = |repo: &std::path::Path| -> Vec<String> {
            configured(repo)
                .unwrap()
                .into_iter()
                .map(|s| s.name)
                .collect()
        };
        assert!(names(repo.path()).contains(&"tax-bas".to_string()), "owed");

        let events = crate::agent_store::agents_root(repo.path())
            .join("assistent")
            .join("events");
        let past = [(
            timers[4].clone(),
            crate::cli::event_source::SourceOrigin::Config,
        )];
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut set =
            crate::cli::timer_source::TimerSet::new(&past, &events, None, tx.into()).unwrap();
        set.arm();
        set.due();
        assert_eq!(
            names(repo.path()),
            [
                "btc-eth-daily-review",
                "daily-inbox-review",
                "garbled",
                "rama-follow-up",
                "virus-meeting"
            ],
            "an unreadable timer is listed, not hidden"
        );
        let sources = configured(repo.path()).unwrap();
        assert!(sources.iter().all(|s| s.timer.is_some()));
        let WaitEventSource::Timer(t) = &sources[0].config else {
            unreachable!()
        };
        assert_eq!(
            sources[0].key,
            crate::cli::timer_source::recurrence::Recurrence::parse(t, None)
                .unwrap()
                .source_key(),
            "a recurring timer's log is its generation's"
        );
    }

    /// A source the monitor only follows is produced by whatever its own
    /// record proves is still running, and by nothing a record cannot
    /// prove: a stale birth, a legacy rendering, no record, a group that
    /// has ended. A source the monitor runs needs no record read.
    #[test]
    fn a_followed_sources_producer_is_what_its_record_proves() {
        use crate::cli::command_source::{Producer, record_producer};
        use crate::cli::event_source::Lifecycle;
        use std::os::unix::process::CommandExt;
        let repo = tempfile::tempdir().unwrap();
        let events = |agent: &str| {
            let dir = crate::agent_store::agents_root(repo.path())
                .join(agent)
                .join("events");
            std::fs::create_dir_all(&dir).unwrap();
            dir
        };
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as i32;
        let birth = crate::proc_identity::token_for(pgid).unwrap();
        let hosted = |agent: &str, key: &str, lifecycle| Hosted {
            agent: agent.into(),
            key: key.into(),
            accepted: "f".into(),
            lifecycle: Some(lifecycle),
            foreign: None,
        };
        let anchored = |birth| Producer::Anchored { pgid, birth };
        record_producer(&events("alpha"), "live", &anchored(birth.clone())).unwrap();
        record_producer(&events("alpha"), "ran", &anchored(birth.clone())).unwrap();
        record_producer(
            &events("alpha"),
            "stale",
            &anchored(crate::proc_identity::ProcToken::MacStart { sec: 1, usec: 1 }),
        )
        .unwrap();
        record_producer(
            &events("alpha"),
            "legacy",
            &Producer::Legacy {
                pgid,
                started: "Thu 1 Oct 16:58:08 2026".into(),
            },
        )
        .unwrap();
        record_producer(&events("beta"), "absent", &anchored(birth)).unwrap();
        let account = Account {
            monitor: 1,
            birth: crate::proc_identity::ProcToken::MacStart { sec: 1, usec: 1 },
            sources: vec![
                hosted("alpha", "live", Lifecycle::Following),
                hosted(
                    "alpha",
                    "ran",
                    Lifecycle::Running {
                        pid: pgid as u32,
                        since: 1,
                    },
                ),
                hosted("alpha", "stale", Lifecycle::Following),
                hosted("alpha", "legacy", Lifecycle::Following),
                hosted("alpha", "absent", Lifecycle::Following),
            ],
        };
        let found = followed_producers(repo.path(), &account);
        assert_eq!(
            found.into_iter().collect::<Vec<_>>(),
            [(("alpha".to_string(), "live".to_string()), pgid)],
            "alpha's absent record is not beta's"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            followed_producers(repo.path(), &account).is_empty(),
            "ended"
        );
    }
}
