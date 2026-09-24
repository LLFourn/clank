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
            anyhow::bail!("no such message `{handle}` in `{label}`'s inbox (see `clank inbox`)")
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

pub fn set_priority(
    repo: &Path,
    label: &AgentLabel,
    handle: &str,
    priority: u16,
) -> anyhow::Result<PathBuf> {
    let message = resolve(repo, label, handle)?;
    priority_dir::set_priority(&inbox_dir(repo, label), &message.entry, priority)
}

/// Put a message in `label`'s inbox and return its path.
///
/// Under the inbox's lock, the next number is reserved in `.seq` —
/// before publishing, so a send that dies half way skips a number and
/// never reuses one. The body is written to a staging file created
/// fresh and exclusively for this send, which the scanner never lists,
/// then published by hard link: renaming a complete file onto a name
/// that is taken would delete the message already there, where a link
/// refuses it.
pub fn send(
    repo: &Path,
    label: &AgentLabel,
    priority: u16,
    from: Option<&Sender>,
    sent: &str,
    body: &str,
) -> anyhow::Result<PathBuf> {
    priority_dir::check_priority(priority)?;
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

#[cfg(test)]
mod tests {
    /// The panel's projection reads the inbox and takes nothing.
    ///
    /// This is the promise the TUI row rests on: the agent is still
    /// the recipient, and a human looking over its shoulder must not
    /// handle its mail by looking. `clank inbox done` is what handles
    /// a message (a-waiting-message-is-a-line-under-the-name).
    #[test]
    fn projecting_an_inbox_for_the_panel_consumes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let label = AgentLabel::parse("claude").unwrap();
        send(
            repo,
            &label,
            500,
            None,
            "2026-09-24T06:44:34Z",
            "## A title\n\nbody",
        )
        .unwrap();
        send(
            repo,
            &label,
            500,
            None,
            "2026-09-24T07:00:00Z",
            "plain subject\n\nmore",
        )
        .unwrap();

        let before = scan(repo, &label);
        assert_eq!(before.len(), 2);

        // What the panel does, twice: the projection is a READ.
        for _ in 0..2 {
            let view = crate::cli::status::InboxView::read(repo, "claude")
                .expect("two messages are waiting");
            assert_eq!(view.messages.len(), 2);
            // The title is the first non-blank line with the heading
            // markers stripped -- the thing the row shows.
            assert_eq!(view.messages[0].subject, "A title");
            assert_eq!(view.messages[1].subject, "plain subject");
            assert!(view.messages.iter().all(|m| m.sent_at.is_some()));
        }

        let after = scan(repo, &label);
        assert_eq!(after.len(), 2, "looking at the inbox must not empty it",);
        assert_eq!(
            before
                .iter()
                .map(|m| m.entry.name.clone())
                .collect::<Vec<_>>(),
            after
                .iter()
                .map(|m| m.entry.name.clone())
                .collect::<Vec<_>>(),
            "and must not reorder or rename anything",
        );
    }

    /// The handle shown is the handle that resolves to THAT message.
    ///
    /// `entry.name` is priority-stripped, so `100-500-note.md` and
    /// `500-note.md` both carry the name `500-note` — and resolving it
    /// picks one of them. A row showing that name would offer a handle
    /// that selects a DIFFERENT message than the one it is describing
    /// (codex 3bc0d05).
    #[test]
    fn the_handle_shown_resolves_to_the_message_it_describes() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let label = AgentLabel::parse("claude").unwrap();
        let inbox = inbox_dir(repo, &label);
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("100-500-note.md"), "FIRST\n\nbody").unwrap();
        std::fs::write(inbox.join("500-note.md"), "SECOND\n\nbody").unwrap();

        let view = crate::cli::status::InboxView::read(repo, "claude").expect("two are waiting");
        assert_eq!(view.messages.len(), 2);
        for m in &view.messages {
            let found = resolve(repo, &label, &m.handle)
                .unwrap_or_else(|e| panic!("`{}` must resolve: {e:#}", m.handle));
            assert_eq!(
                found.subject(),
                m.subject,
                "handle `{}` is shown for `{}` but resolves to `{}`",
                m.handle,
                m.subject,
                found.subject(),
            );
        }
    }

    /// An agent with an EMPTY inbox projects nothing.
    ///
    /// The row exists only where there is mail, and `inbox_rows`
    /// derives the panel's rows from this projection — so a view
    /// returned for an empty inbox would grow a row under every agent
    /// on the roster.
    #[test]
    fn an_empty_inbox_projects_no_view_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let label = AgentLabel::parse("claude").unwrap();
        // Never written to.
        assert!(crate::cli::status::InboxView::read(repo, "claude").is_none());

        // Written to, then handled: still nothing.
        send(
            repo,
            &label,
            500,
            None,
            "2026-09-24T06:44:34Z",
            "a note\n\nbody",
        )
        .unwrap();
        assert!(crate::cli::status::InboxView::read(repo, "claude").is_some());
        let one = scan(repo, &label).pop().expect("it is there");
        std::fs::remove_file(&one.entry.path).unwrap();
        assert!(
            crate::cli::status::InboxView::read(repo, "claude").is_none(),
            "an inbox that empties loses its row",
        );
    }

    /// A message with no `sent:` line still projects, with no age.
    #[test]
    fn a_hand_written_message_projects_without_a_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let label = AgentLabel::parse("claude").unwrap();
        let inbox = inbox_dir(repo, &label);
        std::fs::create_dir_all(&inbox).unwrap();
        // Written by hand, no metadata at all -- which the inbox takes.
        std::fs::write(inbox.join("500-00000001-note.md"), "# Just a note\n\nbody").unwrap();

        let view = crate::cli::status::InboxView::read(repo, "claude").expect("it is waiting");
        assert_eq!(view.messages.len(), 1);
        assert_eq!(view.messages[0].subject, "Just a note");
        assert_eq!(
            view.messages[0].sent_at, None,
            "no stamp is an absence to report, not an age to invent",
        );
    }

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

    fn send_at(repo: &Path, priority: u16, body: &str) -> PathBuf {
        send(repo, &grok(), priority, None, SENT, body).unwrap()
    }

    #[test]
    fn a_priority_reads_in_send_order() {
        let repo = tempfile::tempdir().unwrap();
        send_at(repo.path(), 500, "z-first");
        send_at(repo.path(), 500, "a-second");
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
            send(
                repo.path(),
                &grok(),
                500,
                None,
                SENT,
                &format!("review\n\n{i}"),
            )
            .unwrap();
        }
        let bodies: Vec<String> = scan(repo.path(), &grok())
            .iter()
            .map(|m| m.body.lines().last().unwrap().to_string())
            .collect();
        let expected: Vec<String> = (0..12).map(|i| i.to_string()).collect();
        assert_eq!(bodies, expected, "the tenth after the ninth");
    }

    #[test]
    fn priority_outranks_send_order_and_a_bare_file_sorts_last() {
        let repo = tempfile::tempdir().unwrap();
        send_at(repo.path(), 500, "early");
        send_at(repo.path(), 100, "urgent");
        std::fs::write(inbox_dir(repo.path(), &grok()).join("by-hand.md"), "x").unwrap();
        assert_eq!(
            names(repo.path()),
            ["100-00000002-urgent", "500-00000001-early", "by-hand"]
        );
    }

    #[test]
    fn a_send_leaves_nothing_but_its_message_to_list() {
        let repo = tempfile::tempdir().unwrap();
        let dir = inbox_dir(repo.path(), &grok());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".send-1.tmp"), "half a mess").unwrap();
        send_at(repo.path(), 500, "hello");
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
                    send(&repo, &grok(), 500, None, SENT, &format!("from sender {i}")).unwrap();
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
        let a = send_at(repo.path(), 500, "review please");
        let a_handle = a.file_stem().unwrap().to_string_lossy().into_owned();
        done(repo.path(), &grok(), &a_handle).unwrap();
        let b = send_at(repo.path(), 500, "review please");
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
        assert!(send(repo.path(), &grok(), 500, None, SENT, "hello").is_err());
        assert!(names(repo.path()).is_empty());
    }

    #[test]
    fn numbers_stop_rather_than_wrap() {
        let repo = tempfile::tempdir().unwrap();
        let dir = inbox_dir(repo.path(), &grok());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".seq"), format!("{SEQ_MAX}")).unwrap();
        let err = send(repo.path(), &grok(), 500, None, SENT, "hello").unwrap_err();
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
            500,
            Some(&claude),
            SENT,
            "# Rebase please\n\nonto master",
        )
        .unwrap();
        send_at(repo.path(), 500, "from a person");
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
        let a = send_at(repo.path(), 500, "first");
        let a_bytes = std::fs::read(&a).unwrap();
        let dir = inbox_dir(repo.path(), &grok());
        for leftover in [
            format!(".send-{}.tmp", std::process::id()),
            ".send-leftover.tmp".to_string(),
        ] {
            std::fs::hard_link(&a, dir.join(leftover)).unwrap();
        }

        send_at(repo.path(), 500, "second");

        assert_eq!(std::fs::read(&a).unwrap(), a_bytes, "A is untouched");
        assert_eq!(names(repo.path()).len(), 2);
    }

    #[test]
    fn reprioritising_onto_a_taken_name_keeps_both_messages() {
        let repo = tempfile::tempdir().unwrap();
        let dir = inbox_dir(repo.path(), &grok());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("100-note.md"), "first").unwrap();
        std::fs::write(dir.join("500-note.md"), "second").unwrap();

        assert!(set_priority(repo.path(), &grok(), "500-note", 100).is_err());

        let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();
        assert_eq!(
            (read("100-note.md"), read("500-note.md")),
            ("first".into(), "second".into())
        );
    }

    #[test]
    fn reprioritising_keeps_the_name() {
        let repo = tempfile::tempdir().unwrap();
        send_at(repo.path(), 500, "hello");
        set_priority(repo.path(), &grok(), "00000001-hello", 100).unwrap();
        assert_eq!(names(repo.path()), ["100-00000001-hello"]);
    }
}
