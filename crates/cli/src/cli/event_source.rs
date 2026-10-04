use std::sync::Arc;

use clank_core::{AgentLabel, wait::WaitItem};
use tokio::sync::mpsc::{UnboundedSender, error::SendError};

/// Provenance determines whether a source belongs to an author or one invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceOrigin {
    Config,
    Cli,
}

/// Custody is carried from the append decision, never inferred by the receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Durability {
    Logged,
    Transient,
}

pub(crate) type Emission = (WaitItem, Durability);

/// Success means the delivery owner's queue already holds the emission.
/// Metadata transformations run within the send, with no private buffer or task.
#[derive(Clone)]
pub(crate) struct EmissionSink(
    Arc<dyn Fn(Emission) -> Result<(), SendError<Emission>> + Send + Sync>,
);

impl EmissionSink {
    pub(crate) fn for_author(
        tx: UnboundedSender<(AgentLabel, Emission)>,
        author: AgentLabel,
    ) -> Self {
        Self(Arc::new(move |emission| {
            tx.send((author.clone(), emission))
                .map_err(|e| SendError(e.0.1))
        }))
    }

    pub(crate) fn map(
        self,
        transform: impl Fn(Emission) -> Emission + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(move |emission| self.send(transform(emission))))
    }

    pub(crate) fn send(&self, emission: Emission) -> Result<(), SendError<Emission>> {
        (self.0)(emission)
    }
}

impl From<UnboundedSender<Emission>> for EmissionSink {
    fn from(tx: UnboundedSender<Emission>) -> Self {
        Self(Arc::new(move |emission| tx.send(emission)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emission(id: &str, durability: Durability) -> Emission {
        (
            WaitItem::CommandEvent {
                presentation: None,
                source_cwd: None,
                name: "source".into(),
                id: id.into(),
                event: "source".into(),
                content: id.into(),
                at: None,
                failure: false,
                requires_reply: false,
                reply_route: None,
                selector: None,
            },
            durability,
        )
    }

    #[test]
    fn author_bound_sinks_share_custody_without_sharing_attribution() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let a = AgentLabel::parse("claude").unwrap();
        let b = AgentLabel::parse("codex").unwrap();
        let first = EmissionSink::for_author(tx.clone(), a.clone());
        let second = EmissionSink::for_author(tx, b.clone());
        first.send(emission("a", Durability::Logged)).unwrap();
        second.send(emission("b", Durability::Transient)).unwrap();
        drop(first);
        drop(second);
        let (author, item) = rx.try_recv().unwrap();
        assert_eq!(author, a);
        assert_eq!(item, emission("a", Durability::Logged));
        let (author, item) = rx.try_recv().unwrap();
        assert_eq!(author, b);
        assert_eq!(item, emission("b", Durability::Transient));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn closed_owners_return_the_rejected_emission() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let local = EmissionSink::from(tx);
        drop(rx);
        assert_eq!(
            local
                .send(emission("local", Durability::Transient))
                .unwrap_err()
                .0,
            emission("local", Durability::Transient)
        );
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let hosted = EmissionSink::for_author(tx, AgentLabel::parse("codex").unwrap());
        drop(rx);
        assert_eq!(
            hosted
                .send(emission("hosted", Durability::Logged))
                .unwrap_err()
                .0,
            emission("hosted", Durability::Logged)
        );
    }
}

/// What a client is owed from its author's source logs.
///
/// Read at monitor admission and side-effect-free peek. The producers may
/// have run before this client arrived, and it has been shown none of that work. Forwarding only what comes next would drop the
/// backlog at exactly the moment an agent comes back.
///
/// The rows come from the durable log rather than from anything the
/// monitor remembers, so an ack made by the agent — or by an earlier
/// incarnation of it — is what retires a row, and nothing here can
/// re-present one that was handled.
pub(crate) fn owed_from_sources(
    repo: &std::path::Path,
    author: &AgentLabel,
    sources: &[(
        clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
    )],
    shown: &mut std::collections::HashSet<(String, u64, String)>,
) -> Vec<WaitItem> {
    use clank_core::agent_config::WaitEventSource;

    let events_dir = crate::agent_store::agents_root(repo)
        .join(author.as_str())
        .join("events");
    let mut owed = Vec::new();
    if !events_dir.is_dir() {
        return owed;
    }
    for (source, origin) in sources {
        // Invocation WALs use a separate resolved key and must never borrow
        // the configured source's backlog just because their names match.
        if *origin != crate::cli::event_source::SourceOrigin::Config {
            continue;
        }
        let (key, prompt) = match &source {
            WaitEventSource::Command(c) => (c.source_key(), None),
            WaitEventSource::Timer(_) => continue,
            WaitEventSource::Github(g) => (
                crate::cli::github::supervisor::source_key(g),
                g.prompt.clone(),
            ),
        };
        if !events_dir.join(format!("{key}.jsonl")).is_file() {
            continue;
        }
        // `read_rows` is the READ-ONLY inspector: the monitor holds the
        // ingest lease for these logs, but a reader must never
        // quarantine one, and admission is a read.
        let Ok(log) = crate::cli::github::log::EventLog::open(&events_dir, &key) else {
            continue;
        };
        let Ok(Some(read)) = log.read_rows() else {
            continue;
        };
        owed.extend(crate::cli::command_source::owed_from_log(
            read.rows,
            &key,
            prompt.as_deref(),
            shown,
        ));
    }
    owed
}

#[cfg(test)]
mod backlog_tests {
    use super::*;
    #[test]
    fn configured_backlog_is_read_only_and_invocation_scope_cannot_borrow_it() {
        let fixture = crate::cli::wake_receipt::fixtures::Fixture::new();
        let sources = [(
            clank_core::agent_config::WaitEventSource::Command(fixture.source.clone()),
            SourceOrigin::Config,
        )];
        let events = crate::agent_store::agents_root(&fixture.repo)
            .join(fixture.author.as_str())
            .join("events");
        let wal = events.join(format!("{}.jsonl", fixture.key));
        let before = std::fs::read(&wal).unwrap();
        let mut shown = Default::default();
        let owed = owed_from_sources(&fixture.repo, &fixture.author, &sources, &mut shown);
        assert_eq!(owed, vec![fixture.item.clone()]);
        assert!(owed_from_sources(&fixture.repo, &fixture.author, &sources, &mut shown).is_empty());
        let invoked = [(sources[0].0.clone(), SourceOrigin::Cli)];
        assert!(
            owed_from_sources(
                &fixture.repo,
                &fixture.author,
                &invoked,
                &mut Default::default()
            )
            .is_empty()
        );
        assert_eq!(std::fs::read(wal).unwrap(), before);
        assert!(!fixture.rows()[0].acked);
        assert!(!fixture.output.exists());
    }
    #[test]
    fn inspecting_absent_source_custody_creates_no_agent_event_directory() {
        let fixture = crate::cli::wake_receipt::fixtures::Fixture::new();
        let absent = AgentLabel::parse("absent-author").unwrap();
        let dir = crate::agent_store::agents_root(&fixture.repo)
            .join(absent.as_str())
            .join("events");
        let sources = [(
            clank_core::agent_config::WaitEventSource::Command(fixture.source.clone()),
            SourceOrigin::Config,
        )];
        assert!(
            owed_from_sources(&fixture.repo, &absent, &sources, &mut Default::default()).is_empty()
        );
        assert!(!dir.exists());
    }
}
