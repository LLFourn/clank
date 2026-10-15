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
    /// How a github watch is configured to be delivered.
    pub(crate) delivery: Option<String>,
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
        let (subscription, delivery) = match config {
            WaitEventSource::Github(g) => (
                Some(crate::cli::github::selection::Selection::of(g).describe()),
                Some(crate::cli::github::configured_delivery(g)),
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
            delivery,
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
/// config an agent off the roster left on disk.
pub(crate) fn configured(repo: &std::path::Path) -> Vec<ConfiguredSource> {
    let mut sources: Vec<ConfiguredSource> = crate::agent_store::roster_labels(repo)
        .iter()
        .filter_map(|label| {
            let cfg = crate::agent_store::load_agent_config(repo, label).ok()??;
            Some((label, cfg))
        })
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
    sources
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Hosted {
    pub(crate) agent: String,
    pub(crate) key: String,
    pub(crate) accepted: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) lifecycle: Option<crate::cli::event_source::Lifecycle>,
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
pub(crate) fn publish(repo: &std::path::Path, account: &Account) -> std::io::Result<()> {
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
}

/// Read the account, trusting it only while the monitor that wrote it is
/// the one published for this repo and still the process it was. Never
/// starts a monitor.
pub(crate) fn live(repo: &std::path::Path) -> Live {
    live_in(
        &crate::cli::monitor::runtime_dir(unsafe { libc::getuid() }),
        repo,
    )
}

/// Verified producers of followed sources, by (agent, key).
pub(crate) type Producers = std::collections::BTreeMap<(String, String), i32>;

fn live_in(namespace: &std::path::Path, repo: &std::path::Path) -> Live {
    let crate::cli::monitor::Published::Ours(descriptor) =
        crate::cli::monitor::published_in(namespace, repo, unsafe { libc::getuid() })
    else {
        return Live::Unavailable;
    };
    let Ok(raw) = std::fs::read(path_in(namespace, repo)) else {
        return Live::Unavailable;
    };
    match serde_json::from_slice::<Account>(&raw) {
        Ok(account)
            if account.monitor == descriptor.pid && account.birth.still_holds(account.monitor) =>
        {
            let producers = followed_producers(repo, &account);
            Live::Monitor(account, producers)
        }
        _ => Live::Unavailable,
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
            Live::Unavailable => None,
        }
    }

    /// What the monitor hosts for `source`, if it hosts it.
    pub(crate) fn of(&self, source: &ConfiguredSource) -> Option<&Hosted> {
        match self {
            Live::Monitor(p, _) => p
                .sources
                .iter()
                .find(|h| h.agent == source.agent.as_str() && h.key == source.key),
            Live::Unavailable => None,
        }
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
            }],
        }
    }

    /// The account is believed only while the monitor published for the
    /// repo is the very process that wrote it: no monitor, a monitor
    /// that is not the writer, a writer whose pid was reused, or no
    /// account at all say nothing is known.
    #[test]
    fn the_account_is_the_running_monitors_or_nothing() {
        let me = unsafe { libc::getuid() };
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
        assert!(configured(&repo.path().join("missing")).is_empty());
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
            configured(repo).into_iter().map(|s| s.name).collect()
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
        let sources = configured(repo.path());
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
