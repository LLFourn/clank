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
            WaitEventSource::Timer(t) => (Backend::Timer, t.source_key(None), t.name.clone(), None),
            WaitEventSource::Github(g) => (
                Backend::Github,
                crate::cli::github::supervisor::source_key(g),
                g.repo.clone(),
                None,
            ),
        };
        Self {
            agent: agent.clone(),
            key,
            backend,
            name,
            glyph: crate::cli::log::badge_of(glyph, backend).to_string(),
            config: config.clone(),
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

/// Every source the repo's agents configure, by agent and then name.
/// Messages are always on and need no configuration, so they are not
/// sources here.
pub(crate) fn configured(repo: &std::path::Path) -> Vec<ConfiguredSource> {
    let Ok(agents) = crate::agent_store::load_all_agent_configs(repo) else {
        return Vec::new();
    };
    let mut sources: Vec<ConfiguredSource> = agents
        .iter()
        .flat_map(|(label, cfg)| {
            cfg.wait_events
                .iter()
                .map(move |s| ConfiguredSource::of(label, s))
        })
        .collect();
    sources.sort_by(|a, b| {
        (a.agent.as_str(), &a.name, &a.key).cmp(&(b.agent.as_str(), &b.name, &b.key))
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
    /// The running monitor's own account.
    Monitor(Account),
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
            Live::Monitor(account)
        }
        _ => Live::Unavailable,
    }
}

impl Live {
    /// What the monitor hosts for `source`, if it hosts it.
    pub(crate) fn of(&self, source: &ConfiguredSource) -> Option<&Hosted> {
        match self {
            Live::Monitor(p) => p
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
            Live::Monitor(account(ours))
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

    /// Every configured kind is listed with its timeline icon, by agent
    /// and then name; an unreadable roster lists nothing.
    #[test]
    fn configured_sources_are_listed_by_agent_then_name() {
        let repo = tempfile::tempdir().unwrap();
        for (agent, sources) in [
            ("beta", vec![command("zeta")]),
            ("alpha", vec![command("questions"), command("answers")]),
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
            .map(|s| (s.agent.to_string(), s.name))
            .collect();
        assert_eq!(
            listed,
            [
                ("alpha".to_string(), "answers".to_string()),
                ("alpha".into(), "questions".into()),
                ("beta".into(), "zeta".into()),
            ]
        );
        assert!(configured(&repo.path().join("missing")).is_empty());
    }
}
