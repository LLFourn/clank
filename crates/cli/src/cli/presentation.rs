//! Persist only successful model-delivery boundaries, independently of callbacks.
//! Selection metadata travels with work; viewing the inbox never calls this.
use clank_core::{
    AgentLabel,
    wait::{AnswerRef, EventPresentation},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// What one presentation carried to the model.
#[derive(Debug, Default)]
pub(crate) struct Carried {
    pub(crate) events: Vec<EventPresentation>,
    pub(crate) answers: Vec<AnswerRef>,
    /// The stable identity of every item it presented, event or not.
    pub(crate) identities: Vec<String>,
}

/// What reached the model in one session, by item identity: the only proof
/// that an item was delivered to the binding that holds the label now.
///
/// An event log's `presentation` record names no session, and review work
/// has no log at all, so neither can say which binding saw an item.
#[derive(Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Receipts {
    pub(crate) session: String,
    /// Unix seconds of each item's first delivery to `session`.
    pub(crate) delivered: BTreeMap<String, u64>,
}

/// Receipts kept for one session. Past this the oldest are dropped, whose
/// items read as not delivered and are presented again.
const RECEIPTS_KEPT: usize = 2048;

pub(crate) fn receipts_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("delivered.json")
}

/// When each item reached `session`. Empty when the receipts on disk are
/// another session's, missing, or unreadable: none of those is a delivery.
pub(crate) fn delivered_to(agent_dir: &Path, session: &str) -> BTreeMap<String, u64> {
    std::fs::read(receipts_path(agent_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Receipts>(&bytes).ok())
        .filter(|r| r.session == session)
        .map(|r| r.delivered)
        .unwrap_or_default()
}

/// Record that `identities` reached `session` of `tool` at `at`, if that
/// session holds the label. `false` when it does not, and nothing is
/// written.
///
/// One transaction under the label's binding lock, which `guard` is: the
/// binding is compared and the receipts read, merged and replaced with no
/// bind, rebind or other receipt able to land between. A delivery whose
/// session has since lost the label can therefore never replace its
/// successor's receipts, and two deliveries never lose each other's.
pub(crate) fn receive(
    guard: &crate::agent_store::BindingGuard,
    tool: clank_core::Tool,
    session: &str,
    identities: &[String],
    at: u64,
    permit: &crate::upgrade::Permit,
) -> std::io::Result<bool> {
    receive_with(guard, tool, session, identities, at, permit, |_| {})
}

/// [`receive`], calling `before_write` with the label's directory once the
/// receipts are merged and before they replace the file.
fn receive_with(
    guard: &crate::agent_store::BindingGuard,
    tool: clank_core::Tool,
    session: &str,
    identities: &[String],
    at: u64,
    _permit: &crate::upgrade::Permit,
    before_write: impl FnOnce(&Path),
) -> std::io::Result<bool> {
    if !guard.bound_to(tool, session) {
        return Ok(false);
    }
    if identities.is_empty() {
        return Ok(true);
    }
    let agent_dir = guard.agent_dir();
    let mut delivered = delivered_to(&agent_dir, session);
    for identity in identities {
        delivered.entry(identity.clone()).or_insert(at);
    }
    while delivered.len() > RECEIPTS_KEPT {
        let oldest = delivered
            .iter()
            .min_by_key(|(_, at)| **at)
            .map(|(identity, _)| identity.clone());
        match oldest {
            Some(identity) => delivered.remove(&identity),
            None => break,
        };
    }
    before_write(&agent_dir);
    let receipts = Receipts {
        session: session.to_string(),
        delivered,
    };
    let path = receipts_path(&agent_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&receipts)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(true)
}

/// The one record of a wake that reached the model in `session` of
/// `tool`: its items are delivered to that session if it still holds the
/// label, its events presented and its answers received, each only while
/// `current` holds.
pub(crate) fn record_delivered(
    repo: &Path,
    author: &AgentLabel,
    tool: clank_core::Tool,
    session: &str,
    carried: &Carried,
    current: impl Fn() -> bool,
    permit: &crate::upgrade::Permit,
) {
    let received = match carried.identities.is_empty() {
        true => Ok(true),
        false => crate::agent_store::hold_binding(repo, author)
            .map_err(std::io::Error::other)
            .and_then(|guard| {
                receive(
                    &guard,
                    tool,
                    session,
                    &carried.identities,
                    super::github::log::now_epoch(),
                    permit,
                )
            }),
    };
    if let Err(e) = received {
        eprintln!(
            "clank: wake delivered; could not record its receipt, so it reads as undelivered: {e}"
        );
    }
    record(repo, author, &carried.events, &current, permit);
    for answer in &carried.answers {
        let Ok(session) = clank_core::SessionId::parse(session) else {
            continue;
        };
        let delivered = super::block::Destination {
            agent: author.clone(),
            tool,
            session,
        };
        if let Err(e) = super::block::receive(repo, answer, &delivered, &current, permit) {
            eprintln!(
                "clank: wake delivered; could not receive answer to `{}`, so it stays owed: {e:#}",
                answer.name
            );
        }
    }
}

pub(crate) fn record(
    repo: &Path,
    author: &AgentLabel,
    selected: &[EventPresentation],
    current: impl Fn() -> bool,
    permit: &crate::upgrade::Permit,
) {
    let dir = crate::agent_store::agents_root(repo)
        .join(author.as_str())
        .join("events");
    let mut groups: BTreeMap<&str, Vec<EventPresentation>> = BTreeMap::new();
    for key in selected {
        // Keys come from our wait subprocess, but never let transport metadata
        // select a path outside this author's WAL directory.
        let mut components = Path::new(&key.source).components();
        if !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
            || key.source.contains('\\')
        {
            continue;
        }
        groups.entry(&key.source).or_default().push(key.clone());
    }
    let mut failures = 0;
    for (source, mut keys) in groups {
        if !current() {
            break;
        }
        // Removed custody is never recreated by a late presentation.
        if !super::watch_store::log_exists(&dir, source) {
            continue;
        }
        if let Ok((_, physical_key)) = super::watch_store::log_location(&dir, source) {
            for key in &mut keys {
                key.source = physical_key.clone();
            }
        }
        let result = super::github::log::EventLog::open(&dir, source).and_then(|log| {
            log.under(permit)
                .append_presentations(&keys, super::github::log::now_epoch(), &current)
        });
        if result.is_err() {
            failures += 1;
        }
    }
    if failures > 0 {
        eprintln!(
            "clank: wake delivered; could not persist presentation evidence for {failures} source(s)"
        );
    }
}

#[cfg(test)]
#[path = "presentation_tests.rs"]
mod tests;
