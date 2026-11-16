//! WAL-backed message delivery.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clank_core::ids::AgentLabel;
use clank_core::wait::WaitItem;

use crate::cli::github::log::{EventLog, Transport};

/// The agent and repo a message came from: what its recipient replies
/// to. Absent when a person, not an agent, sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sender {
    pub label: String,
    pub repo: PathBuf,
}

/// Who a message says it is from, as a reader wants it — one wording
/// for the listing, the wait and the wake line.
pub fn sender_words(from: Option<&str>, repo: Option<&str>) -> String {
    match (from, repo) {
        (Some(from), Some(repo)) => format!("from {from} ({repo})"),
        (Some(from), None) => format!("from {from}"),
        (None, _) => "from a person".to_string(),
    }
}

fn first_line(body: &str) -> &str {
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.trim_start_matches('#').trim())
        .unwrap_or("")
}

/// The one source every message in a repo is logged to.
///
/// One source, not one per recipient: the log already lives under the
/// recipient's own directory, so the recipient is the namespace and a
/// second one would only repeat it.
pub(crate) const MESSAGES_SOURCE: &str = "messages";

/// Serializes the allocate-then-append that every writer of this log
/// performs.
///
/// The other producers hold their log for the life of a wait, so one
/// writer per (agent, source) is structural for them. A message has no
/// such owner: every `clank send` is its own process, all appending to
/// the same log. `EventLog::load` reads the next sequence and
/// `append_inbox` writes at it, and two writers that interleave those two
/// steps both take N -- which would advertise one selector, `messages@N`,
/// for two different messages and leave neither separately ackable.
fn message_writer_lock(repo: &Path, label: &AgentLabel) -> anyhow::Result<std::fs::File> {
    let dir = crate::agent_store::agents_root(repo)
        .join(label.as_str())
        .join("events");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating `{}`", dir.display()))?;
    crate::agent_store::exclusive_lock(&dir.join("messages.writer.lock"))
}

/// This agent's message log.
fn message_log(repo: &Path, label: &AgentLabel) -> std::io::Result<EventLog> {
    EventLog::open(
        &crate::agent_store::agents_root(repo)
            .join(label.as_str())
            .join("events"),
        MESSAGES_SOURCE,
    )
}

/// Allocation and append remain one local transaction for every message writer.
/// The closure must return before accessing another repository.
pub(crate) fn message_transaction<T>(
    repo: &Path,
    label: &AgentLabel,
    f: impl FnOnce(&mut EventLog) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    anyhow::ensure!(
        repo.is_dir(),
        "message repository is unavailable: {}",
        repo.display()
    );
    let _writer = message_writer_lock(repo, label)?;
    let mut log = message_log(repo, label)?;
    f(&mut log)
}

/// A name no other message will take.
///
/// Minted rather than counted: `EventLog` is a handle on one (agent,
/// source) pair and numbers only within it, so it cannot tell a send to
/// one agent from a send to another. Random bytes need no allocator and
/// no cross-agent state on the write path.
pub(crate) fn mint_id() -> String {
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).expect("the OS can produce random bytes");
    bytes.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// What `clank events ack` takes for a message, namespaced by its
/// recipient so two agents' messages never merge into one row.
fn action_key(label: &AgentLabel, handle: &str) -> String {
    format!("message#{}/{handle}", label.as_str())
}

/// Log a message to its recipient. Returns the handle it was filed
/// under.
pub fn deliver(
    repo: &Path,
    to: &AgentLabel,
    from: Option<&Sender>,
    sent: &str,
    body: &str,
) -> anyhow::Result<String> {
    deliver_with_options(repo, to, from, sent, body, false, None)
}

pub fn deliver_with_interrupt(
    repo: &Path,
    to: &AgentLabel,
    from: Option<&Sender>,
    sent: &str,
    body: &str,
    interrupt: bool,
) -> anyhow::Result<String> {
    deliver_with_options(repo, to, from, sent, body, interrupt, None)
}

pub fn deliver_with_options(
    repo: &Path,
    to: &AgentLabel,
    from: Option<&Sender>,
    sent: &str,
    body: &str,
    interrupt: bool,
    reply_route: Option<&clank_core::wait::MessageReplyRoute>,
) -> anyhow::Result<String> {
    log_message(
        repo,
        to,
        &mint_id(),
        from,
        sent,
        body,
        interrupt,
        reply_route,
    )
}

#[allow(clippy::too_many_arguments)]
fn log_message(
    repo: &Path,
    to: &AgentLabel,
    handle: &str,
    from: Option<&Sender>,
    sent: &str,
    body: &str,
    interrupt: bool,
    reply_route: Option<&clank_core::wait::MessageReplyRoute>,
) -> anyhow::Result<String> {
    // Held across BOTH steps: reading the next sequence and writing at
    // it are one allocation, and a lock around only the write does not
    // make them one.
    let _writer = message_writer_lock(repo, to)?;
    let mut log = message_log(repo, to)?;
    log.load();
    let item = WaitItem::Message {
        presentation: None,
        handle: handle.to_string(),
        from: from.map(|s| s.label.clone()),
        repo: from.map(|s| s.repo.display().to_string()),
        subject: first_line(body).to_string(),
        body: body.to_string(),
        requires_reply: reply_route.is_some(),
        reply_route: reply_route.cloned(),
        selector: None,
    };
    log.append_interruptible_inbox(
        Transport::Message,
        None,
        Some(&action_key(to, handle)),
        parse_rfc3339_secs(sent),
        &item,
        interrupt.into(),
    )
    .with_context(|| format!("logging a message for `{}`", to.as_str()))?;
    // Compaction runs HERE, on the write path and under the same
    // writer lock, because a message log has no coordinator to run it
    // for it. Every other producer compacts through the loop that owns
    // its source; a message's writers are separate processes that
    // append once and exit, so a log nobody compacts grows forever and
    // the bounded-retention promise -- handled bodies readable until
    // DISPLAY_CAP newer ones displace them -- never comes true.
    //
    // It is cheap when there is nothing to do: `maybe_compact` returns
    // immediately below COMPACT_AT.
    if let Err(e) = log.maybe_compact() {
        eprintln!(
            "clank: could not compact `{}`'s message log: {e}",
            to.as_str()
        );
    }
    Ok(handle.to_string())
}

/// Seconds since the epoch for an RFC3339 stamp, or `None` when it
/// cannot be read. A message whose time cannot be read still arrives;
/// the log dates it itself.
fn parse_rfc3339_secs(s: &str) -> Option<u64> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|t| t.unix_timestamp().max(0) as u64)
}

/// Compact this agent's message log after an ack.
///
/// The message source has no coordinator, so it has to compact on
/// every path that writes it — and an ack is one. A backlog sent, then
/// acked, with no later send would otherwise sit in the WAL forever:
/// `deliver` compacts only while sends keep coming, and nothing else
/// was ever going to.
///
/// It does NOT take the writer lock, and that is deliberate rather
/// than an omission. `maybe_compact` already replaces the file under
/// the exclusive sidecar lock that appenders hold shared, so no append
/// can land mid-rewrite; and it groups rows by `(seq, ident)` without
/// renumbering, so a sequence a sender allocated before the rewrite is
/// still free after it. A second lock here would serialize an
/// operation that is already serialized, and would read as though the
/// first one were insufficient.
pub(crate) fn compact_after_ack(repo: &Path, label: &AgentLabel) {
    let Ok(mut log) = message_log(repo, label) else {
        return;
    };
    log.load();
    if let Err(e) = log.maybe_compact() {
        eprintln!(
            "clank: could not compact `{}`'s message log: {e}",
            label.as_str()
        );
    }
}

/// This agent's unacked messages, oldest first, each stamped with what
/// acks it.
pub fn pending(repo: &Path, label: &AgentLabel) -> Vec<WaitItem> {
    pending_with(repo, label, &crate::cli::github::log::Sidecars::waiting())
}

/// [`pending`], its log read as `sidecars` takes the lock: a held sidecar
/// leaves the messages unavailable this pass, not handled.
pub(crate) fn pending_with(
    repo: &Path,
    label: &AgentLabel,
    sidecars: &crate::cli::github::log::Sidecars,
) -> Vec<WaitItem> {
    let Ok(log) = message_log(repo, label) else {
        return Vec::new();
    };
    let Ok(crate::cli::github::log::Tried::Ran(Some(rows))) =
        sidecars.read_rows(&log, MESSAGES_SOURCE)
    else {
        return Vec::new();
    };
    rows.rows
        .into_iter()
        .filter(|r| !r.acked)
        .filter_map(|row| {
            let mut item = row.item;
            item.set_presentation(clank_core::wait::EventPresentation {
                source: MESSAGES_SOURCE.into(),
                seq: row.seq,
                ident: row.ident,
            });
            let WaitItem::Message { selector, .. } = &mut item else {
                return None;
            };
            *selector =
                Some(crate::cli::events::Selector::qualified(MESSAGES_SOURCE, row.seq).to_string());
            Some(item)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENT: &str = "2026-09-22T10:15:00Z";

    fn grok() -> AgentLabel {
        AgentLabel::parse("grok").unwrap()
    }

    #[test]
    fn interrupt_policy_is_recorded_without_handling_the_message() {
        let repo = tempfile::tempdir().unwrap();
        deliver_with_interrupt(repo.path(), &grok(), None, SENT, "urgent", true).unwrap();
        deliver(repo.path(), &grok(), None, SENT, "ordinary").unwrap();
        let rows = message_log(repo.path(), &grok())
            .unwrap()
            .read_rows()
            .unwrap()
            .unwrap()
            .rows;
        assert!(rows[0].interrupt);
        assert!(!rows[1].interrupt);
        assert_eq!(pending(repo.path(), &grok()).len(), 2);
        assert!(rows.iter().all(|row| row.interrupt_attempt.is_none()));
    }

    /// A MESSAGE IS AN EVENT: it is logged, it wakes until acked, and
    /// acking is what stops it — not the deletion of a file.
    #[test]
    fn a_delivered_message_wakes_until_it_is_acked() {
        let repo = tempfile::tempdir().unwrap();
        let to = grok();
        let from = Sender {
            label: "claude".into(),
            repo: PathBuf::from("/src/clank"),
        };
        let handle = deliver(
            repo.path(),
            &to,
            Some(&from),
            "2026-09-22T10:15:00Z",
            "# Rebase please\n\nonto master",
        )
        .unwrap();

        let woken = pending(repo.path(), &to);
        assert_eq!(woken.len(), 1, "one message, one wake: {woken:?}");
        let WaitItem::Message {
            requires_reply: false,
            reply_route: None,
            handle: got,
            subject,
            from: sender,
            selector,
            ..
        } = &woken[0]
        else {
            panic!("not a message")
        };
        assert_eq!(got, &handle);
        assert_eq!(subject, "Rebase please", "the first line is the subject");
        assert_eq!(sender.as_deref(), Some("claude"));
        assert_eq!(
            selector.as_deref(),
            Some("messages@1"),
            "the wake says what acks it",
        );

        // Reading is not handling: it wakes again.
        assert_eq!(pending(repo.path(), &to).len(), 1, "still owed");

        // Acking is. Same core as every other event.
        let key = crate::cli::github::timeline::MemberKey {
            agent: crate::cli::watch_store::Producer::Agent(to.clone()),
            source: MESSAGES_SOURCE.to_string(),
            seq: 1,
            // `logical_ident` prefixes an action key with `k:`.
            ident: format!("k:{}", action_key(&to, &handle)),
        };
        crate::cli::events::ack_members(repo.path(), &[key]).unwrap();
        assert!(
            pending(repo.path(), &to).is_empty(),
            "an acked message stops waking",
        );
    }

    /// A MESSAGE REACHES THE TIMELINE, as its own kind of row.
    ///
    /// The timeline globs every log in an agent's events directory, so
    /// the message log is picked up without being registered anywhere
    /// — but only if the item produces an event view at all, which
    /// `WaitItem` decided by falling through to `None` until now.
    /// Nothing about "it should just appear" was true before this was
    /// asserted.
    #[test]
    fn a_message_is_a_timeline_row() {
        let repo = tempfile::tempdir().unwrap();
        let to = grok();
        let from = Sender {
            label: "claude".into(),
            repo: PathBuf::from("/src/clank"),
        };
        deliver(
            repo.path(),
            &to,
            Some(&from),
            SENT,
            "# Rebase please\n\nonto master",
        )
        .unwrap();

        let snap = crate::cli::github::timeline::timeline_snapshot(repo.path());
        let row = snap
            .events
            .iter()
            .find(|e| e.kind == "message")
            .expect("the message is in the timeline");
        assert_eq!(
            row.backend,
            crate::cli::github::timeline::Backend::Message,
            "not a command: nobody ran it",
        );
        assert_eq!(row.source, "claude", "the sender is what the row names");
        assert_eq!(row.content.as_deref(), Some("Rebase please"));
        assert!(row.unhandled, "and it is owed until somebody acks it");
    }

    /// Every row of this agent's message log.
    fn rows_of(repo: &Path, label: &AgentLabel) -> Vec<crate::cli::github::log::EventRow> {
        message_log(repo, label)
            .unwrap()
            .read_rows()
            .unwrap()
            .map(|r| r.rows)
            .unwrap_or_default()
    }

    /// CONCURRENT SENDS GET DISTINCT, SEPARATELY ACKABLE SELECTORS.
    ///
    /// Every `clank send` is its own process appending to one log, so
    /// the single-writer assumption the other producers get for free
    /// does not hold here. Two writers that interleave "read the next
    /// sequence" and "write at it" both take N, and then one selector
    /// names two messages and neither can be acked alone.
    #[test]
    fn concurrent_sends_are_separately_ackable() {
        let repo = tempfile::tempdir().unwrap();
        let to = grok();
        let n = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(n));
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let (repo, barrier, to) = (repo.path().to_path_buf(), barrier.clone(), to.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    deliver(&repo, &to, None, SENT, &format!("# Ping {i}\n\nbody")).unwrap()
                })
            })
            .collect();
        let handles: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            handles
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            n,
            "each send got its own handle",
        );

        let woken = pending(repo.path(), &to);
        assert_eq!(woken.len(), n, "all of them are owed: {woken:?}");
        let mut bodies: Vec<String> = woken
            .iter()
            .map(|item| {
                let WaitItem::Message { body, .. } = item else {
                    panic!("message")
                };
                body.clone()
            })
            .collect();
        bodies.sort();
        let mut expected: Vec<String> = (0..n).map(|i| format!("# Ping {i}\n\nbody")).collect();
        expected.sort();
        assert_eq!(
            bodies, expected,
            "concurrent appends preserve every body whole"
        );
        let selectors: std::collections::BTreeSet<String> = woken
            .iter()
            .filter_map(|i| match i {
                WaitItem::Message { selector, .. } => selector.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(
            selectors.len(),
            n,
            "and each says a DIFFERENT thing acks it: {selectors:?}",
        );

        // Each really is separately ackable: acking one leaves the rest.
        let rows = message_log(repo.path(), &to)
            .unwrap()
            .read_rows()
            .unwrap()
            .expect("the log reads back");
        let first = rows.rows.first().expect("a row");
        crate::cli::events::ack_members(
            repo.path(),
            &[crate::cli::github::timeline::MemberKey {
                agent: crate::cli::watch_store::Producer::Agent(to.clone()),
                source: MESSAGES_SOURCE.to_string(),
                seq: first.seq,
                ident: first.ident.clone(),
            }],
        )
        .unwrap();
        assert_eq!(
            pending(repo.path(), &to).len(),
            n - 1,
            "acking one message acks exactly one",
        );
    }

    /// A COMPACTION LANDING UNDER A SENDER COSTS NOTHING.
    ///
    /// With the log already at the threshold, every ack here rewrites
    /// the file while sends are in flight. The invariant is that a
    /// message still lands on a sequence of its own: two on one would
    /// be a selector naming two things.
    ///
    /// It holds because the sidecar lock already keeps a rewrite from
    /// landing mid-append and compaction never renumbers — not because
    /// of a second lock, which is why there is not one. This asserts
    /// the invariant rather than the mechanism, so it stays true if
    /// either changes.
    #[test]
    fn a_compaction_cannot_land_under_a_sender() {
        use crate::cli::github::log::{EventLog, Transport};
        let repo = tempfile::tempdir().unwrap();
        let to = grok();
        let events_dir = crate::agent_store::agents_root(repo.path())
            .join(to.as_str())
            .join("events");

        // Put the log at the threshold so compaction fires on the acks
        // below rather than at some later time.
        let mut log = EventLog::open(&events_dir, MESSAGES_SOURCE).unwrap();
        log.load();
        for i in 0..crate::cli::github::log::COMPACT_AT {
            let seq = log
                .append_inbox(
                    Transport::Message,
                    None,
                    Some(&format!("old#{i}")),
                    None,
                    &WaitItem::Message {
                        presentation: None,
                        requires_reply: false,
                        reply_route: None,
                        handle: format!("o{i}"),
                        from: None,
                        repo: None,
                        subject: "old".into(),
                        body: "old".into(),
                        selector: None,
                    },
                )
                .unwrap();
            log.append_ack(seq).unwrap();
        }
        drop(log);

        let senders = 4;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(senders + 2));
        let mut threads = Vec::new();
        for i in 0..senders {
            let (repo, barrier, to) = (repo.path().to_path_buf(), barrier.clone(), to.clone());
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                Some(deliver(&repo, &to, None, SENT, &format!("# New {i}\n\nbody")).unwrap())
            }));
        }
        for _ in 0..2 {
            let (repo, barrier, to) = (repo.path().to_path_buf(), barrier.clone(), to.clone());
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..4 {
                    let keys: Vec<_> = rows_of(&repo, &to)
                        .into_iter()
                        .filter(|r| !r.acked)
                        .map(|r| crate::cli::github::timeline::MemberKey {
                            agent: crate::cli::watch_store::Producer::Agent(to.clone()),
                            source: MESSAGES_SOURCE.to_string(),
                            seq: r.seq,
                            ident: r.ident,
                        })
                        .collect();
                    let _ = crate::cli::events::ack_members(&repo, &keys);
                }
                None
            }));
        }
        let sent: Vec<String> = threads
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(sent.len(), senders, "every send returned");

        // Every message that was sent is in the log exactly once, and
        // no two rows share a sequence — which is what a rewrite
        // landing under an allocation would break.
        let rows = rows_of(repo.path(), &to);
        for handle in &sent {
            let key = format!("k:{}", action_key(&to, handle));
            assert_eq!(
                rows.iter().filter(|r| r.ident == key).count(),
                1,
                "message {handle} is in the log exactly once",
            );
        }
        let new_seqs: Vec<u64> = rows
            .iter()
            .filter(|r| {
                sent.iter()
                    .any(|h| r.ident == format!("k:{}", action_key(&to, h)))
            })
            .map(|r| r.seq)
            .collect();
        let distinct: std::collections::BTreeSet<_> = new_seqs.iter().collect();
        assert_eq!(
            distinct.len(),
            new_seqs.len(),
            "two messages landed on one sequence: {new_seqs:?}",
        );
    }

    /// A BACKLOG SENT AND THEN ACKED, with no later send, still gets
    /// compacted.
    ///
    /// `deliver` compacts only while sends keep coming. An inbox that
    /// fills up and is then worked through ends on an ACK, and if that
    /// path does not compact, every body and every ack record stays in
    /// the WAL for good -- which is the opposite of the bounded
    /// retention the plan promises.
    #[test]
    fn acking_a_backlog_compacts_it() {
        use crate::cli::github::log::{EventLog, Transport};
        let repo = tempfile::tempdir().unwrap();
        let to = grok();
        let events_dir = crate::agent_store::agents_root(repo.path())
            .join(to.as_str())
            .join("events");
        let log_path = events_dir.join(format!("{MESSAGES_SOURCE}.jsonl"));

        // A backlog, written directly: driving this many real sends is
        // quadratic, and what is under test is the ACK path.
        let mut log = EventLog::open(&events_dir, MESSAGES_SOURCE).unwrap();
        log.load();
        let n = crate::cli::github::log::COMPACT_AT / 2;
        let mut seqs = Vec::new();
        for i in 0..n {
            seqs.push(
                log.append_inbox(
                    Transport::Message,
                    None,
                    Some(&format!("message#{}/{i}", to.as_str())),
                    None,
                    &WaitItem::Message {
                        presentation: None,
                        requires_reply: false,
                        reply_route: None,
                        handle: format!("h{i}"),
                        from: None,
                        repo: None,
                        subject: format!("Ping {i}"),
                        body: "body".into(),
                        selector: None,
                    },
                )
                .unwrap(),
            );
        }
        // All but one already handled, so the log sits just under the
        // threshold and ONE ordinary ack crosses it. Resolving 4000
        // members through the real ack path costs five minutes, and
        // what is under test is the trigger, not the setup.
        for seq in &seqs[..seqs.len() - 1] {
            log.append_ack(*seq).unwrap();
        }
        drop(log);
        let before = std::fs::read_to_string(&log_path).unwrap().lines().count();
        assert_eq!(
            pending(repo.path(), &to).len(),
            1,
            "one left to work through"
        );

        // Worked through, and then NOTHING else happens -- no further
        // send to trigger a compaction on the way in.
        let last = rows_of(repo.path(), &to)
            .into_iter()
            .find(|r| !r.acked)
            .expect("the last unhandled message");
        crate::cli::events::ack_members(
            repo.path(),
            &[crate::cli::github::timeline::MemberKey {
                agent: crate::cli::watch_store::Producer::Agent(to.clone()),
                source: MESSAGES_SOURCE.to_string(),
                seq: last.seq,
                ident: last.ident,
            }],
        )
        .unwrap();

        let after = std::fs::read_to_string(&log_path).unwrap().lines().count();
        assert!(
            after < before,
            "the ack that cleared the backlog must compact it: {before} -> {after}",
        );
        assert!(
            pending(repo.path(), &to).is_empty(),
            "and nothing is owed afterwards",
        );
    }

    /// THE WHOLE MESSAGE REACHES THE READER, before and after the ack.
    ///
    /// Storing the body in the WAL is not the same as being able to
    /// read it: the presentation model carried ONE text field, mapped
    /// to the subject, so every surface showed a first line and the
    /// message itself was unreachable. The row keeps the subject; the
    /// body is carried beside it.
    #[test]
    fn the_whole_message_reaches_the_reader() {
        let repo = tempfile::tempdir().unwrap();
        let to = grok();
        let text = "# Rebase please\n\nonto master, then run the suite.\n\nThanks.";
        deliver(repo.path(), &to, None, SENT, text).unwrap();

        let message = |repo: &Path| {
            crate::cli::github::timeline::timeline_snapshot(repo)
                .events
                .into_iter()
                .find(|e| e.kind == "message")
                .expect("the message is in the timeline")
        };

        let ev = message(repo.path());
        assert_eq!(
            ev.content.as_deref(),
            Some("Rebase please"),
            "the row stays one line",
        );
        assert_eq!(
            ev.body.as_deref(),
            Some(text),
            "and the whole message rides beside it",
        );

        // After the ack too, because a handled message staying readable
        // is the reason for the log.
        crate::cli::events::ack_members(repo.path(), &[ev.members[0].key.clone()]).unwrap();
        assert_eq!(
            message(repo.path()).body.as_deref(),
            Some(text),
            "a handled message is still readable whole",
        );
    }

    /// THE MESSAGE SURVIVES BEING SENT.
    ///
    /// `clank send` leaves no file anywhere now, so the log record is
    /// the only copy: a body that is not in it was not sent. Read back
    /// after the append AND after the ack, because a handled message
    /// staying readable is the point of moving to a log at all.
    #[test]
    fn a_sent_message_can_be_read_back_whole() {
        let repo = tempfile::tempdir().unwrap();
        let to = grok();
        let text = "# Rebase please\n\nonto master, then run the suite.\n\nThanks.";
        let handle = deliver(repo.path(), &to, None, SENT, text).unwrap();

        let body_of = |items: &[WaitItem]| {
            items.iter().find_map(|i| match i {
                WaitItem::Message { body, .. } => Some(body.clone()),
                _ => None,
            })
        };
        assert_eq!(
            body_of(&pending(repo.path(), &to)).as_deref(),
            Some(text),
            "every paragraph of it, not just the first line",
        );

        let key = crate::cli::github::timeline::MemberKey {
            agent: crate::cli::watch_store::Producer::Agent(to.clone()),
            source: MESSAGES_SOURCE.to_string(),
            seq: 1,
            ident: format!("k:{}", action_key(&to, &handle)),
        };
        crate::cli::events::ack_members(repo.path(), &[key]).unwrap();

        let log = message_log(repo.path(), &to).unwrap();
        let rows = log.read_rows().unwrap().expect("the log reads back");
        let handled = rows
            .rows
            .iter()
            .find(|r| r.acked)
            .expect("the acked row is still there");
        let WaitItem::Message { body, .. } = &handled.item else {
            panic!("not a message")
        };
        assert_eq!(body, text, "a handled message is still readable");
    }

    /// Two recipients holding the same handle are two messages.
    ///
    /// The merge groups across agents by source plus key, and every
    /// message shares the one source, so an un-namespaced handle would
    /// alias them into a single row with two copies.
    #[test]
    fn two_recipients_never_share_a_message() {
        let repo = tempfile::tempdir().unwrap();
        let a = AgentLabel::parse("claude").unwrap();
        let b = AgentLabel::parse("codex").unwrap();
        log_message(
            repo.path(),
            &a,
            "same",
            None,
            SENT,
            "for claude",
            false,
            None,
        )
        .unwrap();
        log_message(
            repo.path(),
            &b,
            "same",
            None,
            SENT,
            "for codex",
            false,
            None,
        )
        .unwrap();
        assert_ne!(
            action_key(&a, "same"),
            action_key(&b, "same"),
            "the recipient is part of the identity",
        );
        assert_eq!(pending(repo.path(), &a).len(), 1);
        assert_eq!(pending(repo.path(), &b).len(), 1);
    }

    /// Two sends to ONE recipient are two messages.
    ///
    /// The handle is minted rather than derived from anything the two
    /// sends share, so identical text sent twice is still two things
    /// somebody has to answer.
    #[test]
    fn the_same_message_sent_twice_is_two_messages() {
        let repo = tempfile::tempdir().unwrap();
        let to = grok();
        let one = deliver(repo.path(), &to, None, SENT, "ping").unwrap();
        let two = deliver(repo.path(), &to, None, SENT, "ping").unwrap();
        assert_ne!(one, two, "each send gets its own handle");
        assert_eq!(pending(repo.path(), &to).len(), 2, "both are owed");
    }

    #[test]
    fn sends_read_in_send_order() {
        let repo = tempfile::tempdir().unwrap();
        deliver(repo.path(), &grok(), None, SENT, "z-first").unwrap();
        deliver(repo.path(), &grok(), None, SENT, "a-second").unwrap();
        let subjects: Vec<String> = pending(repo.path(), &grok())
            .into_iter()
            .map(|item| {
                let WaitItem::Message { subject, .. } = item else {
                    panic!("message")
                };
                subject
            })
            .collect();
        assert_eq!(
            subjects,
            ["z-first", "a-second"],
            "send order, not subject order"
        );
    }

    #[test]
    fn twelve_sends_with_one_subject_read_in_send_order() {
        let repo = tempfile::tempdir().unwrap();
        for i in 0..12 {
            deliver(repo.path(), &grok(), None, SENT, &format!("review\n\n{i}")).unwrap();
        }
        let bodies: Vec<String> = pending(repo.path(), &grok())
            .into_iter()
            .map(|item| {
                let WaitItem::Message { body, .. } = item else {
                    panic!("message")
                };
                body.lines().last().unwrap().to_string()
            })
            .collect();
        assert_eq!(bodies, (0..12).map(|i| i.to_string()).collect::<Vec<_>>());
    }

    #[test]
    fn an_acked_selector_never_handles_a_new_message() {
        let repo = tempfile::tempdir().unwrap();
        deliver(repo.path(), &grok(), None, SENT, "review please").unwrap();
        let first = rows_of(repo.path(), &grok()).remove(0);
        let key = crate::cli::github::timeline::MemberKey {
            agent: grok().as_str().into(),
            source: MESSAGES_SOURCE.into(),
            seq: first.seq,
            ident: first.ident,
        };
        crate::cli::events::ack_members(repo.path(), std::slice::from_ref(&key)).unwrap();
        let second = deliver(repo.path(), &grok(), None, SENT, "review please").unwrap();
        crate::cli::events::ack_members(repo.path(), &[key]).unwrap();
        let owed = pending(repo.path(), &grok());
        assert_eq!(owed.len(), 1);
        let WaitItem::Message {
            handle, selector, ..
        } = &owed[0]
        else {
            panic!("message")
        };
        assert_eq!(handle, &second);
        assert_eq!(selector.as_deref(), Some("messages@2"));
    }

    /// A message an earlier clank migrated out of the old file inbox, as
    /// that clank wrote it to the log.
    const MIGRATED: &str = r##"{"t":"inbox","v":1,"seq":1,"at":1794834559,"event_at":1790072100,"transport":"message","action_key":"migrated-message#grok/500-00000001-legacy","item":{"kind":"message","handle":"500-00000001-legacy","from":"claude","repo":"/src/clank","subject":"Rebase please","body":"# Rebase please\n\nonto master\n"}}"##;

    /// A message migrated earlier still reads whole, with its sender, and
    /// is acked like any other; a later send takes the next selector.
    #[test]
    fn a_message_migrated_earlier_still_reads() {
        let repo = tempfile::tempdir().unwrap();
        let to = grok();
        let events = crate::agent_store::agents_root(repo.path()).join("grok/events");
        std::fs::create_dir_all(&events).unwrap();
        std::fs::write(events.join("messages.jsonl"), format!("{MIGRATED}\n")).unwrap();

        let owed = pending(repo.path(), &to);
        let [
            WaitItem::Message {
                from,
                repo: from_repo,
                subject,
                body,
                selector,
                ..
            },
        ] = owed.as_slice()
        else {
            panic!("one message: {owed:?}")
        };
        assert_eq!(from.as_deref(), Some("claude"));
        assert_eq!(from_repo.as_deref(), Some("/src/clank"));
        assert_eq!(subject, "Rebase please");
        assert_eq!(body, "# Rebase please\n\nonto master\n");
        assert_eq!(selector.as_deref(), Some("messages@1"));

        let later = deliver(repo.path(), &to, None, SENT, "later").unwrap();
        let migrated = rows_of(repo.path(), &to).remove(0);
        crate::cli::events::ack_members(
            repo.path(),
            &[crate::cli::github::timeline::MemberKey {
                agent: to.as_str().into(),
                source: MESSAGES_SOURCE.into(),
                seq: migrated.seq,
                ident: migrated.ident,
            }],
        )
        .unwrap();
        let owed = pending(repo.path(), &to);
        let [
            WaitItem::Message {
                handle, selector, ..
            },
        ] = owed.as_slice()
        else {
            panic!("only the later send: {owed:?}")
        };
        assert_eq!(handle, &later);
        assert_eq!(selector.as_deref(), Some("messages@2"));
    }

    /// A send waits for another writer's `messages.writer.lock`, and lands
    /// once it is released.
    #[test]
    fn a_send_waits_for_the_message_writer_lock() {
        let repo = tempfile::tempdir().unwrap();
        let to = grok();
        let writer = message_writer_lock(repo.path(), &to).unwrap();
        let (sent, landed) = std::sync::mpsc::channel();
        let (path, label) = (repo.path().to_path_buf(), to.clone());
        std::thread::spawn(move || {
            let _ = sent.send(deliver(&path, &label, None, SENT, "queued").unwrap());
        });
        assert!(
            landed
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "the send went ahead under another writer's lock"
        );
        assert!(rows_of(repo.path(), &to).is_empty());
        drop(writer);
        let handle = landed
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the send lands once the lock is free");
        assert_eq!(rows_of(repo.path(), &to).len(), 1);
        assert!(matches!(
            &pending(repo.path(), &to)[..],
            [WaitItem::Message { handle: h, .. }] if *h == handle
        ));
    }
}
