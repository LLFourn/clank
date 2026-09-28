//! An agent's inbox: `.clank/agents/<label>/inbox/`, a
//! [`crate::priority_dir`] of messages. A message is its file, and its
//! name — `NNN-<seq>-<slug>` — is both its place in line and its
//! identity: the sequence number comes before the slug, so a priority
//! reads in send order, and it is never reused, so a name that was
//! handled can never come to mean a newer message.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clank_core::ids::AgentLabel;

use crate::priority_dir::{self, Lookup};

const SEQ_DIGITS: usize = 8;
const SEQ_MAX: u64 = 99_999_999;

pub fn inbox_dir(repo: &Path, label: &AgentLabel) -> PathBuf {
    crate::agent_store::agents_root(repo)
        .join(label.as_str())
        .join("inbox")
}

/// The agent and repo a message came from: what its recipient replies
/// to. Absent when a person, not an agent, sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sender {
    pub label: String,
    pub repo: PathBuf,
}

/// One pending message.
#[derive(Debug, Clone)]
pub struct Message {
    pub entry: priority_dir::Entry,
    pub from: Option<String>,
    pub repo: Option<PathBuf>,
    pub sent: Option<String>,
    pub body: String,
}

impl Message {
    /// The body's first non-blank line, without a heading's `#`s.
    pub fn subject(&self) -> &str {
        first_line(&self.body)
    }
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

/// Every pending message in `label`'s inbox, in the order they wake.
pub fn scan(repo: &Path, label: &AgentLabel) -> Vec<Message> {
    priority_dir::scan(&inbox_dir(repo, label))
        .into_iter()
        // A file gone between the listing and the read was handled in
        // between; it is not pending.
        .filter_map(|entry| {
            let raw = std::fs::read_to_string(&entry.path).ok()?;
            Some(parse(entry, &raw))
        })
        .collect()
}

/// The pending message `handle` means: its file stem, else its name.
pub fn resolve(repo: &Path, label: &AgentLabel, handle: &str) -> anyhow::Result<Message> {
    let entries = priority_dir::scan(&inbox_dir(repo, label));
    let entry = match priority_dir::by_handle(&entries, handle) {
        Lookup::One(e) => e.clone(),
        Lookup::None => {
            anyhow::bail!(
                "no such message `{handle}` in `{label}`'s inbox (it may already \
                 have migrated into the event log -- see `clank events list`)"
            )
        }
        Lookup::Many(many) => anyhow::bail!(
            "`{handle}` names {} messages ({}); pass the file stem",
            many.len(),
            many.iter()
                .map(|e| e.file_stem())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    let raw = std::fs::read_to_string(&entry.path)
        .with_context(|| format!("reading `{}`", entry.path.display()))?;
    Ok(parse(entry, &raw))
}

/// Mark a message handled: it leaves the inbox. Returns where it was.
pub fn done(repo: &Path, label: &AgentLabel, handle: &str) -> anyhow::Result<PathBuf> {
    let message = resolve(repo, label, handle)?;
    std::fs::remove_file(&message.entry.path)
        .with_context(|| format!("removing `{}`", message.entry.path.display()))?;
    Ok(message.entry.path)
}

/// Every message sits at one priority. Ordering is ARRIVAL order,
/// which the sequence number in the stem already carries; a per-message
/// knob for reordering an inbox is what
/// `a-message-is-an-event-like-any-other` removes.
pub const SEND_PRIORITY: u16 = 500;

pub fn send(
    repo: &Path,
    label: &AgentLabel,
    from: Option<&Sender>,
    sent: &str,
    body: &str,
) -> anyhow::Result<PathBuf> {
    let priority = SEND_PRIORITY;
    let dir = inbox_dir(repo, label);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating `{}`", dir.display()))?;
    let _lock = crate::agent_store::exclusive_lock(&dir.join(".lock"))?;
    clear_staging(&dir);
    let mut staged = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .suffix(".tmp")
        .tempfile_in(&dir)
        .with_context(|| format!("staging a message in `{}`", dir.display()))?;
    {
        use std::io::Write;
        staged
            .write_all(render(from, sent, body).as_bytes())
            .and_then(|()| staged.as_file().sync_all())
            .with_context(|| format!("writing `{}`", staged.path().display()))?;
    }
    let seq = next_seq(&dir)?;
    publish(&dir, staged.path(), priority, seq, &slug_of(body))
}

const STAGING_PREFIX: &str = ".send-";

/// Remove what earlier sends left staged. A send that died after
/// publishing leaves its staging file as a second name for the message
/// it published; unlinking that name leaves the message whole, and no
/// send ever opens one again. Only called under the inbox lock, so no
/// other send is staging.
fn clear_staging(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(STAGING_PREFIX) && n.ends_with(".tmp"))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Link the complete file `tmp` into `dir` under the first number from
/// `seq` whose name is free, reserving each number it tries.
fn publish(dir: &Path, tmp: &Path, priority: u16, seq: u64, slug: &str) -> anyhow::Result<PathBuf> {
    let mut seq = seq;
    loop {
        reserve(dir, seq)?;
        let name = format!("{seq:0width$}-{slug}", width = SEQ_DIGITS);
        let dest = dir.join(priority_dir::file_name(priority, &name));
        match std::fs::hard_link(tmp, &dest) {
            Ok(()) => return Ok(dest),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => seq += 1,
            Err(e) => {
                return Err(anyhow::anyhow!("publishing `{}`: {e}", dest.display()));
            }
        }
    }
}

/// One past the larger of the last number allocated and the highest
/// pending, so a message written by hand cannot make the counter go
/// back. An unreadable counter is an error, never a guess: guessing low
/// is how a number gets reused.
fn next_seq(dir: &Path) -> anyhow::Result<u64> {
    let path = dir.join(".seq");
    let stored = match std::fs::read_to_string(&path) {
        Ok(s) => s
            .trim()
            .parse::<u64>()
            .with_context(|| format!("`{}` does not hold a message number", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(anyhow::anyhow!("reading `{}`: {e}", path.display())),
    };
    let pending = priority_dir::scan(dir)
        .iter()
        .filter_map(|e| seq_of(&e.name))
        .max()
        .unwrap_or(0);
    Ok(stored.max(pending) + 1)
}

fn reserve(dir: &Path, seq: u64) -> anyhow::Result<()> {
    if seq > SEQ_MAX {
        anyhow::bail!(
            "`{}` has used every message number up to {SEQ_MAX}; nothing more can be sent to it",
            dir.display()
        );
    }
    let tmp = dir.join(".seq.tmp");
    std::fs::write(&tmp, format!("{seq}\n"))
        .with_context(|| format!("writing `{}`", tmp.display()))?;
    std::fs::rename(&tmp, dir.join(".seq"))
        .with_context(|| format!("recording message number {seq} in `{}`", dir.display()))
}

/// The sequence number a message's name leads with, if it has one.
fn seq_of(name: &str) -> Option<u64> {
    let (digits, _) = name.split_once('-')?;
    (digits.len() == SEQ_DIGITS && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

fn first_line(body: &str) -> &str {
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.trim_start_matches('#').trim())
        .unwrap_or("")
}

/// A file-name-safe rendering of the body's first line.
fn slug_of(body: &str) -> String {
    const MAX: usize = 40;
    let mut slug = String::new();
    for c in first_line(body).chars() {
        if slug.len() >= MAX {
            break;
        }
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "message".to_string()
    } else {
        slug.to_string()
    }
}

fn render(from: Option<&Sender>, sent: &str, body: &str) -> String {
    let mut out = String::from("---\n");
    if let Some(s) = from {
        out.push_str(&format!("from: {}\nrepo: {}\n", s.label, s.repo.display()));
    }
    out.push_str(&format!("sent: {sent}\n---\n"));
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Split a message file into its header and body.
///
/// Only a header clank writes is a header: a closed `---` block of
/// nothing but `from`, `repo` and `sent`, with `sent` present. Anything
/// else at the top — a hand-written message that opens with a markdown
/// rule — is body, all of it.
fn parse(entry: priority_dir::Entry, raw: &str) -> Message {
    let whole = |entry| Message {
        entry,
        from: None,
        repo: None,
        sent: None,
        body: raw.to_string(),
    };
    let Some(rest) = raw.strip_prefix("---\n") else {
        return whole(entry);
    };
    let (mut from, mut repo, mut sent) = (None, None, None);
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        let line = line.trim_end_matches('\n');
        if line == "---" {
            let Some(sent) = sent else {
                return whole(entry);
            };
            return Message {
                entry,
                from,
                repo,
                sent: Some(sent),
                body: rest[offset..].to_string(),
            };
        }
        match line.split_once(": ") {
            Some(("from", v)) => from = Some(v.to_string()),
            Some(("repo", v)) => repo = Some(PathBuf::from(v)),
            Some(("sent", v)) => sent = Some(v.to_string()),
            _ => return whole(entry),
        }
    }
    whole(entry)
}

// ── a message is an event (a-message-is-an-event-like-any-other) ──
// Everything above this line is the FILE inbox, which exists now only
// so messages written before this change can be migrated out of it.

use crate::cli::github::log::{EventLog, Transport};
use clank_core::wait::WaitItem;

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
/// such owner: every `clank send` is its own process, and migration is
/// another, all appending to the same log. `EventLog::load` reads the
/// next sequence and `append_inbox` writes at it, and two writers that
/// interleave those two steps both take N -- which would advertise one
/// selector, `messages@N`, for two different messages and leave
/// neither separately ackable.
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

/// A name no other message will take.
///
/// Minted rather than counted: `EventLog` is a handle on one (agent,
/// source) pair and numbers only within it, so it cannot tell a send to
/// one agent from a send to another. Random bytes need no allocator and
/// no cross-agent state on the write path.
fn mint_id() -> String {
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

/// The same, for a message MIGRATED out of the old file inbox.
///
/// A different prefix because it is a different promise: this one's
/// source file can outlive its append, so its identity is pinned in
/// the log's dedup horizon and must be recognisable there
/// (`log::MIGRATED_MESSAGE_PREFIX`). A new send needs no pin, because
/// nothing replays it.
fn migrated_action_key(label: &AgentLabel, handle: &str) -> String {
    format!(
        "{}{}/{handle}",
        crate::cli::github::log::MIGRATED_MESSAGE_PREFIX,
        label.as_str()
    )
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
    log_message(repo, to, &mint_id(), from, sent, body, None)
}

/// The shared append, used by a send and by migration. `at` overrides
/// the event's display time, which migration needs and a send does not.
fn log_message(
    repo: &Path,
    to: &AgentLabel,
    handle: &str,
    from: Option<&Sender>,
    sent: &str,
    body: &str,
    at: Option<u64>,
) -> anyhow::Result<String> {
    let key = action_key(to, handle);
    log_message_with_key(repo, to, handle, &key, from, sent, body, at)
}

/// The same, under a caller-chosen action key. Migration needs its own
/// so the identity can be pinned; a send takes the ordinary one.
#[allow(clippy::too_many_arguments)]
fn log_message_with_key(
    repo: &Path,
    to: &AgentLabel,
    handle: &str,
    key: &str,
    from: Option<&Sender>,
    sent: &str,
    body: &str,
    at: Option<u64>,
) -> anyhow::Result<String> {
    // Held across BOTH steps: reading the next sequence and writing at
    // it are one allocation, and a lock around only the write does not
    // make them one.
    let _writer = message_writer_lock(repo, to)?;
    let mut log = message_log(repo, to)?;
    log.load();
    let item = WaitItem::Message {
        handle: handle.to_string(),
        from: from.map(|s| s.label.clone()),
        repo: from.map(|s| s.repo.display().to_string()),
        subject: first_line(body).to_string(),
        body: body.to_string(),
        selector: None,
    };
    let at = at.or_else(|| parse_rfc3339_secs(sent));
    log.append_inbox(Transport::Message, None, Some(key), at, &item)
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

/// Move every message still sitting in this agent's FILE inbox into
/// its log, oldest first, and take the directory with them.
///
/// Called on every read, and free when there is nothing to do: the
/// inbox directory stops existing once it has been emptied, so its
/// absence is the record that this has already run.
///
/// ORDER is `priority_dir::scan`'s own — the order the inbox has always
/// read in, and defined for every file it accepts, including one with
/// no sequence number at all. The event's DATE is the best thing each
/// file actually has: the time it records, else its modification time
/// (an observation about the file, not a claim about the message),
/// else now, which places an undateable file last rather than giving
/// it an arrival it cannot support.
///
/// CUSTODY is append, durable, then delete — never the other order,
/// which is the one way a message is lost beyond recovery. A crash
/// between the two leaves the file and the event both, and the next
/// run finds the ident already in the log and deletes the file without
/// minting a second message.
pub fn migrate(repo: &Path, label: &AgentLabel) -> anyhow::Result<usize> {
    let dir = inbox_dir(repo, label);
    if !dir.is_dir() {
        return Ok(0);
    }
    let _lock = crate::agent_store::exclusive_lock(&dir.join(".lock"))?;
    clear_staging(&dir);

    // The log's own dedup HORIZON, not its rows. Compaction folds an
    // old handled record into an identity observation and drops the
    // row, so a reader that asked the rows would stop recognising a
    // message it had already taken -- and mint a second one from the
    // file that a crash left behind. Migrated identities are pinned in
    // that horizon and never age out of it.
    let taken: std::collections::BTreeSet<String> = match message_log(repo, label) {
        Ok(mut log) => log.load().action_keys.into_iter().collect(),
        Err(_) => Default::default(),
    };

    let mut moved = 0;
    for entry in priority_dir::scan(&dir) {
        let path = entry.path.clone();
        let Ok(raw) = std::fs::read_to_string(&path) else {
            // Unreadable now may be readable later; leaving it is the
            // only answer that cannot lose it.
            continue;
        };
        let at = mtime_secs(&path);
        let message = parse(entry, &raw);
        let handle = message.entry.file_stem();
        let key = migrated_action_key(label, &handle);
        if !taken.contains(&key) {
            let sender = message.from.as_ref().map(|label| Sender {
                label: label.clone(),
                repo: message.repo.clone().unwrap_or_default(),
            });
            let at = message
                .sent
                .as_deref()
                .and_then(parse_rfc3339_secs)
                .or(at)
                .or_else(|| parse_rfc3339_secs(&crate::cli::stop_hook::now_rfc3339()));
            log_message_with_key(
                repo,
                label,
                &handle,
                &key,
                sender.as_ref(),
                "",
                &message.body,
                at,
            )?;
        }
        std::fs::remove_file(&path).with_context(|| format!("removing `{}`", path.display()))?;
        moved += 1;
    }

    // The directory goes only when every MESSAGE has gone -- a file
    // left behind unread keeps it, because its presence is the next
    // run's work list. What remains then is clank's own bookkeeping,
    // the lock and the sequence counter, which has nothing left to
    // count. A real inbox always has both, so removing only the lock
    // left every migrated directory standing and every read repeating
    // the migration.
    if priority_dir::scan(&dir).is_empty() {
        drop(_lock);
        for name in [".lock", ".seq", ".seq.tmp"] {
            let _ = std::fs::remove_file(dir.join(name));
        }
        if let Err(e) = std::fs::remove_dir(&dir) {
            // Something this does not own is in there. Leave it, and
            // leave the directory: deleting a stranger's file to tidy
            // up is not a trade this is allowed to make.
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "clank: migrated every message out of `{}`, but it is not empty: {e}",
                    dir.display()
                );
            }
        }
    }
    Ok(moved)
}

/// Migrate every agent's file inbox in this repo.
///
/// The READ boundary. Migration used to hang off `pending`, which is
/// the WAIT path — so a legacy message reached the timeline only once
/// its recipient next waited, and until then a person looking at the
/// TUI or `clank events list` saw no trace of mail that was genuinely
/// owed. Readers go straight to the WALs, so a reader has to be a
/// place migration happens too.
///
/// Best-effort and free when there is nothing to do: an agent whose
/// inbox directory is gone costs one failed `is_dir`.
pub fn migrate_all(repo: &Path) {
    let Ok(agents) = std::fs::read_dir(crate::agent_store::agents_root(repo)) else {
        return;
    };
    for agent in agents.flatten() {
        let Some(name) = agent.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(label) = AgentLabel::parse(&name) else {
            continue;
        };
        let _ = migrate(repo, &label);
    }
}

/// A file's modification time in epoch seconds, when it can be read.
fn mtime_secs(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
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
    // Anything still in the old file inbox joins the log first, so one
    // read answers for both.
    let _ = migrate(repo, label);
    let Ok(log) = message_log(repo, label) else {
        return Vec::new();
    };
    let Ok(Some(rows)) = log.read_rows() else {
        return Vec::new();
    };
    rows.rows
        .into_iter()
        .filter(|r| !r.acked)
        .filter_map(|row| {
            let mut item = row.item;
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

    fn names(repo: &Path) -> Vec<String> {
        scan(repo, &grok())
            .iter()
            .map(|m| m.entry.file_stem())
            .collect()
    }

    fn send_at(repo: &Path, body: &str) -> PathBuf {
        send(repo, &grok(), None, SENT, body).unwrap()
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
            agent: to.as_str().to_string(),
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

    /// A LEGACY MESSAGE IS VISIBLE TO A READER, without anyone having
    /// waited first.
    ///
    /// Migration used to hang off `pending`, which is the wait path.
    /// With the old panel gone, that left a person opening the TUI or
    /// `clank events list` seeing no trace of mail that was genuinely
    /// owed, until its recipient happened to wait. A reader has to be
    /// a place migration happens, so this drives the reader and never
    /// calls `pending` at all.
    #[test]
    fn a_reader_sees_a_legacy_message_without_a_wait() {
        let repo = tempfile::tempdir().unwrap();
        let to = grok();
        let dir = inbox_dir(repo.path(), &to);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("500-00000001-rebase.md"),
            "# Rebase please\n\nonto master",
        )
        .unwrap();

        // The TIMELINE, which is what the TUI draws.
        let snap = crate::cli::github::timeline::timeline_snapshot(repo.path());
        let row = snap
            .events
            .iter()
            .find(|e| e.kind == "message")
            .expect("a reader migrates before it reads");
        assert_eq!(row.content.as_deref(), Some("Rebase please"));
        assert!(row.unhandled, "and it is still owed");
        assert!(!dir.exists(), "the file moved rather than being copied");
    }

    /// EVERY FILE THE INBOX ACCEPTS MIGRATES, including the ones it
    /// accepts without metadata.
    ///
    /// `priority_dir::scan` takes any `*.md`, so a hand-dropped
    /// `500-note.md` with no stamp and no sequence number is a valid
    /// message. It cannot be dated from its contents and cannot be
    /// ordered by a sequence it does not have, which is why the rule
    /// is the scan's own order and the best date each file actually
    /// has.
    #[test]
    fn migration_takes_every_file_the_inbox_accepts() {
        let repo = tempfile::tempdir().unwrap();
        let to = grok();
        let dir = inbox_dir(repo.path(), &to);
        std::fs::create_dir_all(&dir).unwrap();
        // Stamped, as `send` used to write them.
        std::fs::write(
            dir.join("500-00000001-stamped.md"),
            format!("---\nfrom: claude\nrepo: /src/clank\nsent: {SENT}\n---\n\n# Stamped\n\nbody"),
        )
        .unwrap();
        // Hand-written: no metadata at all.
        std::fs::write(dir.join("500-00000002-plain.md"), "# Plain\n\nbody").unwrap();
        // Hand-dropped: not even a sequence number.
        std::fs::write(dir.join("500-note.md"), "# Note\n\nbody").unwrap();

        assert_eq!(migrate(repo.path(), &to).unwrap(), 3, "all three move");
        assert!(!dir.exists(), "the directory goes with them");

        let woken = pending(repo.path(), &to);
        let subjects: Vec<&str> = woken
            .iter()
            .filter_map(|i| match i {
                WaitItem::Message { subject, .. } => Some(subject.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            subjects,
            ["Stamped", "Plain", "Note"],
            "in the order the inbox read them: {woken:?}",
        );
        let bodies: Vec<&str> = woken
            .iter()
            .filter_map(|i| match i {
                WaitItem::Message { body, .. } => Some(body.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            bodies.iter().all(|b| b.contains("body")),
            "each keeps its text: {bodies:?}",
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
                agent: to.as_str().to_string(),
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
                            agent: to.as_str().to_string(),
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

    /// THE OTHER SEAM: a crash BEFORE the append landed, resumed after
    /// the horizon has moved on.
    ///
    /// The first seam leaves a file whose event exists. This one leaves
    /// a file whose event does not — the append never got there — and
    /// the resumed run must produce exactly one message rather than
    /// none. It is the seam that fails in the opposite direction: an
    /// over-eager dedup, or an identity recorded before the write it
    /// stands for, would drop the message entirely.
    #[test]
    fn a_migration_that_never_appended_still_delivers_once() {
        use crate::cli::github::log::{EventLog, Transport};
        let repo = tempfile::tempdir().unwrap();
        let to = grok();
        let events_dir = crate::agent_store::agents_root(repo.path())
            .join(to.as_str())
            .join("events");

        // Unrelated traffic, compacted, so the horizon has moved well
        // past anything this message could be confused with.
        let mut noise = EventLog::open(&events_dir, MESSAGES_SOURCE).unwrap();
        noise.load();
        for i in 0..crate::cli::github::log::COMPACT_AT {
            let seq = noise
                .append_inbox(
                    Transport::Message,
                    None,
                    Some(&format!("noise#{i}")),
                    None,
                    &WaitItem::Message {
                        handle: format!("n{i}"),
                        from: None,
                        repo: None,
                        subject: "noise".into(),
                        body: "noise".into(),
                        selector: None,
                    },
                )
                .unwrap();
            noise.append_ack(seq).unwrap();
        }
        noise.maybe_compact().unwrap();
        drop(noise);

        // The crash: the file is here and its event never was.
        let dir = inbox_dir(repo.path(), &to);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("500-00000001-late.md"), "# Late\n\nbody").unwrap();

        assert_eq!(migrate(repo.path(), &to).unwrap(), 1, "the file is taken");
        let owed: Vec<String> = pending(repo.path(), &to)
            .into_iter()
            .filter_map(|i| match i {
                WaitItem::Message { subject, .. } => Some(subject),
                _ => None,
            })
            .collect();
        assert_eq!(
            owed.iter().filter(|s| *s == "Late").count(),
            1,
            "exactly one -- not none, and not two: {owed:?}",
        );
        assert!(!dir.exists(), "and the inbox is gone");
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
                agent: to.as_str().to_string(),
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

    /// THE INTERRUPTION RESUMED ACROSS COMPACTION still mints one
    /// message.
    ///
    /// This is the case the ordinary dedup horizon cannot answer, and
    /// the reason migrated identities are pinned in it. A crash between
    /// the durable append and the file's deletion leaves the file to be
    /// found whenever the next read happens -- and nothing schedules
    /// that read, so a repo busy with unrelated events can push the
    /// horizon past the message in between. Driven, not argued: the
    /// events are actually arrived and the log is actually compacted.
    #[test]
    fn a_migration_resumed_after_compaction_mints_one_message() {
        use crate::cli::github::log::{EventLog, Transport};
        let repo = tempfile::tempdir().unwrap();
        let to = grok();
        let dir = inbox_dir(repo.path(), &to);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("500-00000001-ping.md");
        std::fs::write(&file, "# Ping\n\nbody").unwrap();

        assert_eq!(migrate(repo.path(), &to).unwrap(), 1);
        let ev = crate::cli::github::timeline::timeline_snapshot(repo.path())
            .events
            .into_iter()
            .find(|e| e.kind == "message")
            .expect("migrated");
        // Handled, so compaction may fold it away.
        crate::cli::events::ack_members(repo.path(), &[ev.members[0].key.clone()]).unwrap();

        // Enough unrelated handled events to push the horizon well past
        // it, and a compaction to act on them.
        let events_dir = crate::agent_store::agents_root(repo.path())
            .join(to.as_str())
            .join("events");
        // Enough handled noise to put the log over the compaction
        // threshold. Written directly because driving COMPACT_AT real
        // sends costs four minutes: every send reads the whole log to
        // learn its next sequence, so building the backlog that way is
        // quadratic. The compaction itself is still triggered by a
        // REAL send below -- that is the path under test.
        let mut noise = EventLog::open(&events_dir, MESSAGES_SOURCE).unwrap();
        noise.load();
        for i in 0..crate::cli::github::log::COMPACT_AT {
            let item = WaitItem::Message {
                handle: format!("n{i}"),
                from: None,
                repo: None,
                subject: "noise".into(),
                body: "noise".into(),
                selector: None,
            };
            let seq = noise
                .append_inbox(
                    Transport::Message,
                    None,
                    Some(&format!("noise#{i}")),
                    None,
                    &item,
                )
                .unwrap();
            noise.append_ack(seq).unwrap();
        }
        let log_path = events_dir.join(format!("{MESSAGES_SOURCE}.jsonl"));
        let lines_before = std::fs::read_to_string(&log_path).unwrap().lines().count();

        // ONE ordinary send, through the path a person uses. Nothing
        // here calls compaction by hand.
        deliver(repo.path(), &to, None, SENT, "# Trigger\n\nbody").unwrap();
        let lines_after = std::fs::read_to_string(&log_path).unwrap().lines().count();
        assert!(
            lines_after < lines_before,
            "an ordinary send must compact a log this size: {lines_before} -> {lines_after}",
        );

        // The crash window: the file was never deleted.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "# Ping\n\nbody").unwrap();
        assert_eq!(migrate(repo.path(), &to).unwrap(), 1, "the file is taken");

        // The message was acked before the noise, so compaction has
        // folded it away -- that is ordinary and fine. What must NOT
        // happen is a second one appearing, which is what a replay
        // produces and what an unhandled `Ping` would be.
        let owed: Vec<String> = pending(repo.path(), &to)
            .into_iter()
            .filter_map(|i| match i {
                WaitItem::Message { subject, .. } => Some(subject),
                _ => None,
            })
            .collect();
        assert!(
            !owed.iter().any(|s| s == "Ping"),
            "the crash resumed after compaction minted a second message: {owed:?}",
        );
        assert!(!file.exists(), "and the file was taken this time");
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

    /// A REAL INBOX MIGRATES AWAY COMPLETELY.
    ///
    /// Built by the send path rather than by hand, so it has what a
    /// historical inbox actually has: a `.seq` counter and a `.lock`
    /// beside the messages. Hand-made fixtures have neither, which is
    /// how a cleanup that removed only the lock passed while leaving
    /// every real directory standing and every read re-migrating it.
    #[test]
    fn a_real_inbox_leaves_nothing_behind() {
        let repo = tempfile::tempdir().unwrap();
        let to = grok();
        let from = Sender {
            label: "claude".into(),
            repo: PathBuf::from("/src/clank"),
        };
        send(repo.path(), &to, Some(&from), SENT, "# First\n\nbody").unwrap();
        send(repo.path(), &to, None, SENT, "# Second\n\nbody").unwrap();
        let dir = inbox_dir(repo.path(), &to);
        assert!(dir.join(".seq").exists(), "the send path keeps a counter");

        assert_eq!(migrate(repo.path(), &to).unwrap(), 2);
        assert!(
            !dir.exists(),
            "a migrated inbox leaves no directory: {:?}",
            std::fs::read_dir(&dir)
                .map(|d| d.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
                .unwrap_or_default(),
        );

        let woken = pending(repo.path(), &to);
        assert_eq!(woken.len(), 2, "both are owed");
        // And a second read does no work, because there is nothing left
        // to find.
        assert_eq!(migrate(repo.path(), &to).unwrap(), 0);
    }

    /// AN UNREADABLE FILE IS LEFT, not dropped.
    ///
    /// Unreadable now may be readable later — a permission fixed, a
    /// disk that came back — so leaving it is the only answer that
    /// cannot lose a message. The directory stays with it, which is
    /// what tells the next run there is still something to do.
    #[test]
    fn migration_leaves_a_file_it_cannot_read() {
        let repo = tempfile::tempdir().unwrap();
        let to = grok();
        let dir = inbox_dir(repo.path(), &to);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("500-00000001-fine.md"), "# Fine\n\nbody").unwrap();
        let locked = dir.join("500-00000002-locked.md");
        std::fs::write(&locked, "# Locked\n\nbody").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        }

        let moved = migrate(repo.path(), &to).unwrap();
        assert_eq!(moved, 1, "only the readable one moved");
        assert!(locked.exists(), "the unreadable message is still there");
        assert!(dir.exists(), "and its directory stays, so this runs again");
        assert!(
            dir.join(".lock").exists(),
            "and so does the lock: tearing it down while a message is \
             still here would leave the next two migrations nothing to \
             exclude each other with",
        );
        assert_eq!(pending(repo.path(), &to).len(), 1);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert_eq!(
            migrate(repo.path(), &to).unwrap(),
            1,
            "readable now, taken now"
        );
        assert_eq!(pending(repo.path(), &to).len(), 2);
        assert!(!dir.exists(), "and now the directory goes");
    }

    /// MIGRATION IS AT-LEAST-ONCE. Run it twice, or interrupt it after
    /// the append and before the delete, and the message exists once.
    ///
    /// The interruption is DRIVEN rather than argued: the file is put
    /// back after a completed migration, which is exactly the state a
    /// crash in that window leaves behind.
    #[test]
    fn an_interrupted_migration_mints_no_second_message() {
        let repo = tempfile::tempdir().unwrap();
        let to = grok();
        let dir = inbox_dir(repo.path(), &to);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("500-00000001-ping.md");
        std::fs::write(&file, "# Ping\n\nbody").unwrap();

        assert_eq!(migrate(repo.path(), &to).unwrap(), 1);
        assert_eq!(pending(repo.path(), &to).len(), 1);

        // The crash window: the event is durable, the file never went.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "# Ping\n\nbody").unwrap();
        assert_eq!(migrate(repo.path(), &to).unwrap(), 1, "the file is taken");
        assert_eq!(
            pending(repo.path(), &to).len(),
            1,
            "but no second message is minted",
        );
        assert!(!dir.exists(), "and the directory goes again");

        // And a run with nothing to do is not an error.
        assert_eq!(migrate(repo.path(), &to).unwrap(), 0);
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
            agent: to.as_str().to_string(),
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
        log_message(repo.path(), &a, "same", None, SENT, "for claude", None).unwrap();
        log_message(repo.path(), &b, "same", None, SENT, "for codex", None).unwrap();
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
        send_at(repo.path(), "z-first");
        send_at(repo.path(), "a-second");
        assert_eq!(
            names(repo.path()),
            ["500-00000001-z-first", "500-00000002-a-second"],
            "send order, not subject order"
        );
    }

    #[test]
    fn twelve_sends_with_one_subject_read_in_send_order() {
        let repo = tempfile::tempdir().unwrap();
        for i in 0..12 {
            send(repo.path(), &grok(), None, SENT, &format!("review\n\n{i}")).unwrap();
        }
        let bodies: Vec<String> = scan(repo.path(), &grok())
            .iter()
            .map(|m| m.body.lines().last().unwrap().to_string())
            .collect();
        let expected: Vec<String> = (0..12).map(|i| i.to_string()).collect();
        assert_eq!(bodies, expected, "the tenth after the ninth");
    }

    /// Send order is the only order, now that every message sits at one
    /// priority — and a hand-dropped file with no sequence at all still
    /// has a place, which migration depends on.
    #[test]
    fn sends_read_in_order_and_a_bare_file_sorts_last() {
        let repo = tempfile::tempdir().unwrap();
        send_at(repo.path(), "early");
        send_at(repo.path(), "urgent");
        std::fs::write(inbox_dir(repo.path(), &grok()).join("by-hand.md"), "x").unwrap();
        assert_eq!(
            names(repo.path()),
            ["500-00000001-early", "500-00000002-urgent", "by-hand"]
        );
    }

    #[test]
    fn a_send_leaves_nothing_but_its_message_to_list() {
        let repo = tempfile::tempdir().unwrap();
        let dir = inbox_dir(repo.path(), &grok());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".send-1.tmp"), "half a mess").unwrap();
        send_at(repo.path(), "hello");
        assert_eq!(names(repo.path()), ["500-00000001-hello"]);
    }

    /// A writer that bypassed the lock has taken the name this send
    /// would publish under. Linking refuses the name; renaming onto it
    /// would have deleted that writer's message.
    #[test]
    fn publishing_never_replaces_a_message() {
        let repo = tempfile::tempdir().unwrap();
        let dir = inbox_dir(repo.path(), &grok());
        std::fs::create_dir_all(&dir).unwrap();
        let theirs = dir.join("500-00000001-hello.md");
        std::fs::write(&theirs, "theirs").unwrap();
        let tmp = dir.join(".send-test.tmp");
        std::fs::write(&tmp, "mine").unwrap();

        let dest = publish(&dir, &tmp, 500, 1, "hello").unwrap();

        assert_eq!(dest, dir.join("500-00000002-hello.md"));
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "mine");
    }

    #[test]
    fn senders_released_together_all_arrive_whole() {
        let repo = tempfile::tempdir().unwrap();
        let n = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(n));
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let (repo, barrier) = (repo.path().to_path_buf(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    send(&repo, &grok(), None, SENT, &format!("from sender {i}")).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let mut bodies: Vec<String> = scan(repo.path(), &grok())
            .into_iter()
            .map(|m| m.body)
            .collect();
        bodies.sort();
        let mut expected: Vec<String> = (0..n).map(|i| format!("from sender {i}\n")).collect();
        expected.sort();
        assert_eq!(bodies, expected);
    }

    #[test]
    fn a_handled_name_never_comes_to_mean_a_newer_message() {
        let repo = tempfile::tempdir().unwrap();
        let a = send_at(repo.path(), "review please");
        let a_handle = a.file_stem().unwrap().to_string_lossy().into_owned();
        done(repo.path(), &grok(), &a_handle).unwrap();
        let b = send_at(repo.path(), "review please");
        let b_bytes = std::fs::read(&b).unwrap();

        assert_ne!(a, b, "the drained inbox did not reuse A's number");
        let late = done(repo.path(), &grok(), &a_handle).unwrap_err();
        assert!(late.to_string().contains("no such message"), "{late}");
        assert_eq!(std::fs::read(&b).unwrap(), b_bytes, "B is untouched");
    }

    #[test]
    fn a_counter_it_cannot_read_stops_the_send() {
        let repo = tempfile::tempdir().unwrap();
        let dir = inbox_dir(repo.path(), &grok());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".seq"), "not a number").unwrap();
        assert!(send(repo.path(), &grok(), None, SENT, "hello").is_err());
        assert!(names(repo.path()).is_empty());
    }

    #[test]
    fn numbers_stop_rather_than_wrap() {
        let repo = tempfile::tempdir().unwrap();
        let dir = inbox_dir(repo.path(), &grok());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".seq"), format!("{SEQ_MAX}")).unwrap();
        let err = send(repo.path(), &grok(), None, SENT, "hello").unwrap_err();
        assert!(err.to_string().contains("every message number"), "{err}");
        assert!(names(repo.path()).is_empty());
    }

    #[test]
    fn the_header_names_its_sender_or_nobody() {
        let repo = tempfile::tempdir().unwrap();
        let claude = Sender {
            label: "claude".to_string(),
            repo: PathBuf::from("/src/clank"),
        };
        send(
            repo.path(),
            &grok(),
            Some(&claude),
            SENT,
            "# Rebase please\n\nonto master",
        )
        .unwrap();
        send_at(repo.path(), "from a person");
        let messages = scan(repo.path(), &grok());

        assert_eq!(messages[0].from.as_deref(), Some("claude"));
        assert_eq!(messages[0].repo.as_deref(), Some(Path::new("/src/clank")));
        assert_eq!(messages[0].sent.as_deref(), Some(SENT));
        assert_eq!(messages[0].subject(), "Rebase please");
        assert_eq!(messages[0].body, "# Rebase please\n\nonto master\n");
        assert_eq!(messages[0].entry.name, "00000001-rebase-please");

        assert_eq!(
            (messages[1].from.as_ref(), messages[1].repo.as_ref()),
            (None, None)
        );
        assert_eq!(messages[1].sent.as_deref(), Some(SENT));
    }

    #[test]
    fn a_hand_written_message_is_all_body() {
        let repo = tempfile::tempdir().unwrap();
        let dir = inbox_dir(repo.path(), &grok());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.md"), "---\nnot closed\n").unwrap();
        let m = &scan(repo.path(), &grok())[0];
        assert_eq!((m.from.as_ref(), m.sent.as_ref()), (None, None));
        assert_eq!(m.body, "---\nnot closed\n");
    }

    #[test]
    fn an_opening_rule_is_body_not_a_header() {
        let repo = tempfile::tempdir().unwrap();
        let dir = inbox_dir(repo.path(), &grok());
        std::fs::create_dir_all(&dir).unwrap();
        let ruled = "---\nImportant first section\n---\nSecond section\n";
        std::fs::write(dir.join("ruled.md"), ruled).unwrap();
        let unsent = "---\nfrom: claude\n---\nbody\n";
        std::fs::write(dir.join("unsent.md"), unsent).unwrap();
        let messages = scan(repo.path(), &grok());
        for (m, raw) in messages.iter().zip([ruled, unsent]) {
            assert_eq!(m.body, raw, "{}", m.entry.name);
            assert_eq!((m.from.as_ref(), m.sent.as_ref()), (None, None));
        }
    }

    /// A send that died after publishing left its staging file as a
    /// second name for the message it published. A later send must not
    /// write through that name.
    #[test]
    fn a_leftover_staging_file_is_never_written_through() {
        let repo = tempfile::tempdir().unwrap();
        let a = send_at(repo.path(), "first");
        let a_bytes = std::fs::read(&a).unwrap();
        let dir = inbox_dir(repo.path(), &grok());
        for leftover in [
            format!(".send-{}.tmp", std::process::id()),
            ".send-leftover.tmp".to_string(),
        ] {
            std::fs::hard_link(&a, dir.join(leftover)).unwrap();
        }

        send_at(repo.path(), "second");

        assert_eq!(std::fs::read(&a).unwrap(), a_bytes, "A is untouched");
        assert_eq!(names(repo.path()).len(), 2);
    }
}
