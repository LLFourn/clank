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
