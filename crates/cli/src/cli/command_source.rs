//! A command source's dedup index and its line format
//! (a-command-is-an-event-source-like-any-other).
//!
//! The split here is the plan's: the BACKEND owns its source — how
//! it acquires events, what it caches, what counts as new, and all
//! filtering. Its one obligation is an `id` unique within the
//! source, and it may emit the same event more than once.
//!
//! Clank owns this agent's relationship to each event: whether it is
//! in the inbox, whether the id has been seen before, whether it is
//! handled. The inbox is the same `EventLog` github writes, so
//! `clank events list/show/ack` finds a command event by the same
//! directory glob. Only the id index lives here, because the WAL's
//! identity retention is bounded and this contract is not.

pub(crate) use super::source_ids::{Known, Seen};

/// The private directory a backend keeps its own state in —
/// `.clank/agents/<label>/sources/<name>/`, handed over in
/// `CLANK_SOURCE_STATE_DIR`.
///
/// Clank creates it and NEVER reads it. Whatever cursor, cache or
/// baseline a backend needs lives here, which is the counterpart of
/// clank no longer holding a resume point on its behalf.
/// Takes the RESOLVED ownership key — the same one that names the log,
/// the lease and the index — and not the source's own. A `--event`
/// source scoped away from the author's configured one would otherwise
/// still share its private directory: a configured `deploys` producer
/// and a CLI `deploys` producer would run at once on separate logs and
/// one `CLANK_SOURCE_STATE_DIR`, where a stateful backend can consume
/// the other invocation's cursor or overwrite its cache and lose an
/// event before it ever reaches the isolated log (codex on 03cc11f).
/// One key owns every piece of a source's state.
pub(crate) fn backend_state_dir(agent_dir: &std::path::Path, key: &str) -> std::path::PathBuf {
    // The SOURCE KEY, not a slug of the name. A slug is lossy the
    // same way it was for the key — `ci/deploy` and `ci-deploy`
    // collide, and two backends would share one private cache — and
    // worse here, because a name of `..` slugs to `..` and resolves
    // to the agent directory itself (codex 1948041). The key is
    // `command-<slug>-<hash>`, which is a single safe component by
    // construction.
    agent_dir.join("sources").join(key)
}

/// One line of a producer's stdout, as the wire carries it.
///
/// Presentation is NOT here: the glyph is config, so a producer
/// cannot reach the renderer's layout.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct Line {
    /// Per-event override; absence inherits the source's default.
    #[serde(default)]
    pub(crate) interrupt: Option<bool>,
    /// Per-event override of the source's interruption description.
    #[serde(default)]
    pub(crate) interrupt_message: Option<String>,
    /// Producer-owned and unique within the source. Compared for
    /// EQUALITY and nothing else — never ordered, never a resume
    /// point.
    pub(crate) id: String,
    /// Labels the event. Optional: it defaults to the source's name,
    /// because with no filtering in clank it only labels.
    #[serde(default)]
    pub(crate) kind: Option<String>,
    /// The text a reader sees. Optional: an event with no content
    /// still renders, from its kind and source.
    #[serde(default)]
    pub(crate) content: Option<String>,
    /// The producer's own timestamp; ingest time when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) at: Option<u64>,
    /// An answer must be accepted by the backend before this event is handled.
    #[serde(default)]
    pub(crate) requires_reply: bool,
}

/// Why a line was refused. Refusing is loud and per-line: one bad
/// line must not cost every later event, so the stream continues.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// Not JSON, or not an object with the required fields.
    Malformed(String),
    /// Parsed, but carries no usable identity — without one there is
    /// nothing to dedup on.
    NoId,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Malformed(why) => write!(f, "not a JSON event object: {why}"),
            Refused::NoId => write!(f, "no `id`: nothing to identify the event by"),
        }
    }
}

/// Read one line. Blank lines are skipped, not refused — a producer
/// flushing a newline is not an error.
pub(crate) fn parse_line(raw: &str) -> Option<Result<Line, Refused>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let line: Line = match serde_json::from_str(raw) {
        Ok(l) => l,
        Err(e) => {
            // An object missing `id`/`kind` is a CONTRACT failure, not
            // a syntax one, and says so — the two get fixed
            // differently.
            return Some(Err(match serde_json::from_str::<serde_json::Value>(raw) {
                Ok(v) if v.get("id").is_none() => Refused::NoId,
                _ => Refused::Malformed(e.to_string()),
            }));
        }
    };
    if line.id.trim().is_empty() {
        return Some(Err(Refused::NoId));
    }
    Some(Ok(line))
}

/// A safe, visible icon for display, or the backend's default badge.
/// This is deliberately not a source-admission check. A cosmetic field
/// cannot stop an event producer or an agent's hook-owned wait.
pub(crate) fn display_glyph(glyph: &str) -> Option<&str> {
    use unicode_segmentation::UnicodeSegmentation;

    let mut graphemes = glyph.graphemes(true);
    if graphemes.next().is_none()
        || graphemes.next().is_some()
        || glyph.chars().any(|c| c.is_control() || c.is_whitespace())
        || crate::cli::status_tui::text::display_width(glyph) == 0
    {
        return None;
    }
    Some(glyph)
}

/// How long to wait before restarting a producer that stopped.
///
/// Doubling from [`BACKOFF_FIRST`] to [`BACKOFF_CAP`]. `failures` is
/// the count of consecutive failures, so the FIRST restart is prompt
/// — a producer that exits cleanly on a deploy should come back
/// quickly, and only a source that keeps dying is made to wait.
pub(crate) fn backoff(failures: u32) -> std::time::Duration {
    let shift = failures.saturating_sub(1).min(16);
    let secs = BACKOFF_FIRST.as_secs().saturating_mul(1u64 << shift);
    std::time::Duration::from_secs(secs.min(BACKOFF_CAP.as_secs()))
}

pub(crate) const BACKOFF_FIRST: std::time::Duration = std::time::Duration::from_secs(1);
pub(crate) const BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(300);

/// Consecutive failures before a source's inability to run becomes an
/// EVENT rather than a silence.
///
/// An agent waiting on a producer that will never start should find
/// out; waiting forever on a typo in an argv is the failure this
/// number exists to end.
pub(crate) const FAILURES_BEFORE_SURFACING: u32 = 3;

/// A run long enough to count as healthy, after which the failure
/// streak resets.
///
/// Without this a producer that is merely restarted nightly creeps to
/// the cap and stays there, looking fine while taking five minutes to
/// come back.
pub(crate) const HEALTHY_RUN: std::time::Duration = std::time::Duration::from_secs(60);

/// How often a non-holder re-reads the holder's log and re-tries the
/// lease.
///
/// It bounds two things at once: how stale a follower's view of the
/// inbox gets, and how long a takeover waits after the holder
/// leaves. Cheap — a file read and a `flock` attempt, no process.
pub(crate) const TAIL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// The kind clank gives its OWN failure reports. A producer cannot
/// forge one: its lines are refused if they claim this kind, so
/// "the source is broken" can never be something the source said.
pub(crate) const FAILURE_KIND: &str = "clank.source-failure";

/// What one line means, before anything durable happens.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Ingested {
    /// A new event. The id travels WITH the payload so the caller
    /// never re-derives it — two places deriving one identity is two
    /// places to disagree.
    Emit {
        interrupt: crate::cli::github::log::Interrupt,
        id: String,
        item: Box<clank_core::wait::WaitItem>,
    },
    /// This id is already an event of this agent's. Do nothing:
    /// logging it again would put a second copy in the inbox. The
    /// event it duplicates is unaffected and is still re-presented
    /// until it is acked.
    Duplicate,
    /// Refused, and nothing is remembered about it: there is no id to
    /// remember, or the line claims clank's own failure kind. The
    /// producer may fix it and re-send. One bad line costs only
    /// itself — the stream continues.
    Refuse(String),
}

/// Decide what to do with one raw line.
///
/// Clank does NO filtering: what counts as an event is the backend's
/// business, and a `kind` here only labels. All that is decided is
/// whether this id is already an event of this agent's.
pub(crate) fn ingest_line(
    src: &clank_core::agent_config::CommandSource,
    seen: &Seen,
    raw: &str,
) -> Option<Ingested> {
    let line = match parse_line(raw)? {
        Ok(l) => l,
        Err(why) => return Some(Ingested::Refuse(why.to_string())),
    };
    let kind = line.kind.filter(|k| !k.trim().is_empty());
    if kind.as_deref() == Some(FAILURE_KIND) {
        return Some(Ingested::Refuse(format!(
            "`{FAILURE_KIND}` is clank's own kind; a source cannot report itself broken"
        )));
    }
    match seen.known(&line.id) {
        Known::Yes => return Some(Ingested::Duplicate),
        Known::Damaged(why) => {
            // Reported, and treated as NOT seen: the replay that
            // follows rewrites both the index entry and the event.
            eprintln!("wait: source `{}` has a damaged id entry — {why}", src.name);
        }
        Known::No => {}
    }
    Some(Ingested::Emit {
        interrupt: crate::cli::github::log::Interrupt::new(
            line.interrupt.unwrap_or(src.interrupt),
            line.interrupt_message
                .as_deref()
                .or(src.interrupt_message.as_deref()),
        ),
        id: line.id.clone(),
        item: Box::new(clank_core::wait::WaitItem::CommandEvent {
            presentation: None,
            source_cwd: None,
            name: src.name.clone(),
            id: line.id,
            // Defaults to the source's name: with nothing filtering on
            // it, a producer that omits it means "one of mine".
            event: kind.unwrap_or_else(|| src.name.clone()),
            // Untrusted, so terminal control goes — but this is the
            // STORED body, and every later surface reads it. A row
            // preview here (it was `one_line`) kept the first line and
            // threw the rest away before the event existed, so nothing
            // downstream could ever recover a second paragraph.
            content: crate::cli::status_tui::text::safe_multiline(
                line.content.as_deref().unwrap_or(""),
            ),
            at: line.at,
            failure: false,
            requires_reply: line.requires_reply,
            reply_route: None,
            // Stamped by `apply` once the row it names exists.
            selector: None,
        }),
    })
}

/// Carry out one decision, in the ORDER that makes a crash safe.
///
/// The LOG first — it is the durable event and the thing an ack
/// finishes — then the id index, then the presentation. Reversed,
/// the index would suppress the producer's only chance to offer
/// again an event that was never stored: at-most-once, which is the
/// bug the whole plan exists to avoid (codex a43bd85).
///
/// Returns false when the sink is gone and the source should stop.
pub(crate) fn apply(
    what: Ingested,
    source_key: &str,
    log: &mut crate::cli::github::log::EventLog,
    seen: &mut Seen,
    send: impl FnOnce(clank_core::wait::WaitItem) -> bool,
) -> bool {
    match what {
        Ingested::Refuse(_) | Ingested::Duplicate => true,
        Ingested::Emit {
            id,
            mut item,
            interrupt,
        } => {
            // The producer's timestamp goes in the ENVELOPE, where
            // every reader already looks for an event's own time.
            // Leaving it None made a replayed or backfilled event
            // display as ingested-now and sort as new (codex
            // 80e1fb9); the timeline also falls back to the item's
            // `at` for rows written before this.
            let event_at = match &*item {
                clank_core::wait::WaitItem::CommandEvent { at, .. } => *at,
                _ => None,
            };
            let seq = match log.append_interruptible_inbox(
                crate::cli::github::log::Transport::Command,
                Some(&id),
                None,
                event_at,
                &item,
                interrupt,
            ) {
                Ok(seq) => seq,
                Err(e) => {
                    // Unlogged means unindexed, so the producer's
                    // next replay gets to try again.
                    eprintln!("wait: could not log event `{id}`: {e}");
                    return true;
                }
            };
            // A crash between the two leaves a logged event whose id
            // is unindexed. The WAL's own identity dedup covers the
            // replay while it is in range, and past that a duplicate
            // in the inbox beats a lost event.
            if let Err(e) = seen.record(&id) {
                eprintln!("wait: could not index event id `{id}`: {e}");
            }
            item.set_presentation(log.presentation_key(
                seq,
                crate::cli::github::log::Transport::Command,
                Some(&id),
                None,
                event_at,
                &item,
            ));
            stamp_selector(&mut item, source_key, seq);
            send(*item)
        }
    }
}

/// Re-apply the fields a row carries only when PRESENTED.
///
/// Neither of these is in the log, and for the same reason: both are
/// facts about where a row is being read from rather than about the
/// row. A command event's ack selector is derived from its log and
/// sequence. A github watch's `prompt` is stamped at presentation and
/// deliberately kept OUT of the WAL bytes, so editing the watch
/// retitles events already logged — which also means a backlog
/// replayed straight from the log reaches the agent with its
/// instructions missing unless they are put back here (codex on
/// 09a9cc8).
pub(crate) fn stamp_presentation(
    item: &mut clank_core::wait::WaitItem,
    source_key: &str,
    seq: u64,
    prompt: Option<&str>,
) {
    match item {
        clank_core::wait::WaitItem::CommandEvent { .. }
        | clank_core::wait::WaitItem::TimerEvent { .. } => stamp_selector(item, source_key, seq),
        clank_core::wait::WaitItem::GithubEvent { instructions, .. } => {
            *instructions = prompt.map(str::to_string);
        }
        _ => {}
    }
}

/// Tell a presented event how to be acked.
///
/// Stamped here rather than stored, exactly as github stamps a
/// watch's `instructions`: the selector is a property of the LOG ROW,
/// not of the event, and appending a row cannot name its own seq
/// before it has one.
///
/// Source-QUALIFIED, because a bare seq is ambiguous the moment
/// another source's WAL in the same directory reaches the same
/// number, and the ingest side cannot see those.
pub(crate) fn stamp_selector(item: &mut clank_core::wait::WaitItem, source_key: &str, seq: u64) {
    if let clank_core::wait::WaitItem::CommandEvent { selector, .. }
    | clank_core::wait::WaitItem::TimerEvent { selector, .. } = item
    {
        *selector = Some(crate::cli::events::Selector::qualified(source_key, seq).to_string());
    }
}

/// Where a source's durable state lives — log, ingest lease and index.
///
/// A CONFIG source is an AUTHOR's: one key, shared by every reader of
/// that author, which is exactly what lets one holder own the producer
/// and every other reader tail its log.
///
/// A `--event` source belongs to ONE INVOCATION, and
/// `CommandSource::source_key` is derived from the NAME alone — so a
/// CLI `deploys` and a configured `deploys` would share a log and a
/// lease. With a monitor permanently holding that lease, the
/// invocation's task tails the monitor's log and never runs the
/// command it was given; and two invocations of the same name for one
/// author would receive each other's events (codex on a135276). The
/// invocation goes in the key, so neither can happen.
pub(crate) fn storage_key(
    key: String,
    origin: crate::cli::event_source::SourceOrigin,
    invocation: &str,
) -> String {
    use crate::cli::event_source::SourceOrigin;
    match origin {
        SourceOrigin::Config => key,
        SourceOrigin::Cli => format!("{key}-inv-{invocation}"),
    }
}

/// What a client is owed from a source's durable log.
///
/// `shown` is PER CLIENT, and that is the whole point. When a parked
/// wait owned its own producer, "what has been presented" and "what
/// this agent has seen" were the same set, because the process was
/// both. A monitor outlives every client, so a set it kept for itself
/// would mean a client admitted later is told nothing about rows
/// logged before it arrived — the backlog silently dropped at exactly
/// the moment an agent comes back.
///
/// So exactly-once is per CLIENT ADMISSION, not per row for all time.
/// An unacked row is owed to each client that has not been shown it;
/// being shown to a client that then died is not being handled, and
/// only a durable ack retires a row.
///
/// The `shown` key carries the SOURCE as well, so one set per client
/// is correct however many sources that client has: a `(seq, ident)`
/// pair belongs to one WAL, and two sources numbering from 1 would
/// otherwise suppress each other's first rows (codex on 09a9cc8).
/// Scoping that a caller has to remember is scoping a caller gets
/// wrong, so the identity carries it instead.
///
/// Pure over the rows so the decision is testable without a producer,
/// a lease, or a log on disk.
pub(crate) fn owed_from_log(
    rows: Vec<crate::cli::github::log::EventRow>,
    source_key: &str,
    prompt: Option<&str>,
    shown: &mut std::collections::HashSet<(String, u64, String)>,
) -> Vec<clank_core::wait::WaitItem> {
    let mut owed = Vec::new();
    for row in rows {
        // An acked row is finished with, whoever acked it. A row this
        // client has already been handed is not owed twice.
        if row.acked || !shown.insert((source_key.to_string(), row.seq, row.ident.clone())) {
            continue;
        }
        let mut item = row.item;
        item.set_presentation(clank_core::wait::EventPresentation {
            source: source_key.to_owned(),
            seq: row.seq,
            ident: row.ident,
        });
        stamp_presentation(&mut item, source_key, row.seq, prompt);
        owed.push(item);
    }
    owed
}

/// Report that a source cannot stay up — unless an earlier report is
/// still outstanding.
///
/// ONE unacked failure row per source. Not deduped through [`Seen`]:
/// that is permanent, and a source that breaks, is acknowledged, and
/// breaks again must be able to say so. Not one row per streak
/// either — an unacked row already wakes the agent on every arm, so
/// a second is a nag, not information.
///
/// Returns false when the sink is gone.
pub(crate) fn report_failure(
    src: &clank_core::agent_config::CommandSource,
    source_key: &str,
    log: &mut crate::cli::github::log::EventLog,
    seen: &mut Seen,
    why: &str,
    send: impl FnOnce(clank_core::wait::WaitItem) -> bool,
) -> bool {
    if outstanding_failure(log) {
        return true;
    }
    // Distinct per report, so two genuinely separate breakages are
    // two rows rather than one the index would swallow. The gate
    // above is what keeps that from becoming a pile.
    let id = format!(
        "{FAILURE_KIND}:{source_key}:{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    apply(
        Ingested::Emit {
            interrupt: Default::default(),
            item: Box::new(failure_event(src, &id, why)),
            id,
        },
        source_key,
        log,
        seen,
        send,
    )
}

/// Whether this source's log already holds an unhandled failure
/// report. A read failure answers NO: silence about a broken source
/// is the worse of the two mistakes.
fn outstanding_failure(log: &crate::cli::github::log::EventLog) -> bool {
    matches!(log.read_rows(), Ok(Some(read)) if read.rows.iter().any(|r| {
        !r.acked
            && matches!(
                r.item,
                clank_core::wait::WaitItem::CommandEvent { failure: true, .. }
            )
    }))
}

/// The item clank emits when a source cannot stay up.
fn failure_event(
    src: &clank_core::agent_config::CommandSource,
    id: &str,
    why: &str,
) -> clank_core::wait::WaitItem {
    clank_core::wait::WaitItem::CommandEvent {
        presentation: None,
        source_cwd: None,
        name: src.name.clone(),
        id: id.to_string(),
        event: FAILURE_KIND.to_string(),
        content: crate::cli::status_tui::text::safe_multiline(why),
        at: None,
        failure: true,
        requires_reply: false,
        reply_route: None,
        selector: None,
    }
}

/// Attach the configuration of the producer that supplied this event, before
/// its single durable inbox append. Backlog and config edits never replace it.
pub(crate) fn ingest_line_with_route(
    src: &clank_core::agent_config::CommandSource,
    seen: &Seen,
    raw: &str,
    route: Option<&clank_core::wait::CommandReplyRoute>,
) -> Option<Ingested> {
    let mut what = ingest_line(src, seen, raw)?;
    if let Ingested::Emit { item, .. } = &mut what
        && let clank_core::wait::WaitItem::CommandEvent {
            requires_reply,
            reply_route,
            source_cwd,
            ..
        } = item.as_mut()
    {
        *reply_route = route.cloned();
        *source_cwd = std::env::current_dir().ok();
        if *requires_reply && reply_route.is_none() {
            eprintln!(
                "wait: source `{}` emitted an event requiring a reply without a reply_command; it stays owed",
                src.name
            );
        }
    }
    Some(what)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::github::log::EventLog;
    use clank_core::wait::WaitItem;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn src_named(name: &str) -> clank_core::agent_config::CommandSource {
        clank_core::agent_config::CommandSource {
            interrupt: false,
            interrupt_message: None,
            name: name.into(),
            command: vec!["true".into()],
            reply_command: None,
            receipt_command: None,
            glyph: None,
        }
    }

    #[test]
    fn producer_override_and_source_default_are_snapshotted_before_replay() {
        let dir = tmpdir();
        let (mut log, mut seen) = store(dir.path());
        let mut source = src_named("voice");
        source.interrupt = true;
        for raw in [
            r#"{"id":"one","content":"wake"}"#,
            r#"{"id":"two","interrupt":false}"#,
        ] {
            let what = ingest_line(&source, &seen, raw).unwrap();
            assert!(apply(what, KEY, &mut log, &mut seen, |_| true));
        }
        let rows = log.read_rows().unwrap().unwrap().rows;
        assert!(rows[0].interrupt);
        assert!(!rows[1].interrupt);
        source.interrupt = false;
        let explicit = ingest_line(&source, &seen, r#"{"id":"three","interrupt":true}"#).unwrap();
        assert!(apply(explicit, KEY, &mut log, &mut seen, |_| true));
        assert!(log.read_rows().unwrap().unwrap().rows[2].interrupt);
        assert_eq!(
            ingest_line(&source, &seen, r#"{"id":"one","interrupt":false}"#),
            Some(Ingested::Duplicate)
        );
        assert!(log.read_rows().unwrap().unwrap().rows[0].interrupt);
    }

    /// A line's description overrides its source's, and an unusable one is
    /// still an event: it is stored as given, and only ever refused where a
    /// hint is typed.
    #[test]
    fn a_lines_description_overrides_its_sources_and_never_refuses_the_event() {
        let dir = tmpdir();
        let (mut log, mut seen) = store(dir.path());
        let mut source = src_named("voice");
        source.interrupt = true;
        source.interrupt_message = Some("Interrupted for new voice message".into());
        for raw in [
            r#"{"id":"one"}"#,
            r#"{"id":"two","interrupt_message":"Urgent voice message"}"#,
            r#"{"id":"three","interrupt_message":"two\nlines"}"#,
            r#"{"id":"four","interrupt":false,"interrupt_message":"ignored"}"#,
        ] {
            let what = ingest_line(&source, &seen, raw).unwrap();
            assert!(apply(what, KEY, &mut log, &mut seen, |_| true));
        }
        let rows = log.read_rows().unwrap().unwrap().rows;
        let hints: Vec<_> = rows.iter().map(|r| r.interrupt_hint.as_deref()).collect();
        assert_eq!(
            hints,
            [
                Some("Interrupted for new voice message"),
                Some("Urgent voice message"),
                Some("two\nlines"),
                None,
            ]
        );
        assert!(rows[2].interrupt, "an unusable description keeps its event");
    }

    const KEY: &str = "command-s-00000000";

    /// The pair the supervisor holds: the shared inbox WAL and this
    /// source's id index.
    fn store(dir: &std::path::Path) -> (EventLog, Seen) {
        (EventLog::open(dir, KEY).unwrap(), Seen::at(dir, KEY))
    }

    fn unacked(log: &EventLog) -> Vec<WaitItem> {
        log.read_rows()
            .unwrap()
            .expect("log readable")
            .rows
            .into_iter()
            .filter(|r| !r.acked)
            .map(|r| r.item)
            .collect()
    }

    /// A duplicate id never becomes a SECOND logical event, and its
    /// age does not matter.
    ///
    /// This is why `Seen` exists beside the WAL rather than being
    /// replaced by it: the WAL's own identity retention is capped at
    /// `LOG_IDENTITY_CAP`, so an id far enough back has aged out of
    /// it entirely — and the test asserts that, so it cannot pass by
    /// the WAL doing the deduplication.
    #[test]
    fn a_duplicate_is_never_a_second_event_however_old() {
        let dir = tmpdir();
        let (mut log, mut seen) = store(dir.path());
        let src = src_named("s");

        let first = r#"{"id":"first","kind":"k","content":"x"}"#;
        let what = ingest_line(&src, &seen, first).expect("not blank");
        assert!(matches!(what, Ingested::Emit { .. }));
        apply(what, KEY, &mut log, &mut seen, |_| true);

        // Far past the WAL's identity cap.
        for n in 0..(crate::cli::github::log::LOG_IDENTITY_CAP * 3) {
            let raw = format!(r#"{{"id":"n{n}","kind":"k"}}"#);
            let what = ingest_line(&src, &seen, &raw).expect("not blank");
            apply(what, KEY, &mut log, &mut seen, |_| true);
        }

        let mut reopened = EventLog::open(dir.path(), KEY).unwrap();
        assert!(
            !reopened.load().feed_ids.iter().any(|f| f == "first"),
            "the WAL must have forgotten this id, or it is doing the dedup",
        );
        assert_eq!(
            ingest_line(&src, &seen, first).expect("not blank"),
            Ingested::Duplicate,
            "the index must still recognise it: nothing ages out of `Seen`",
        );
    }

    /// An unacked event survives the arm, and the producer replaying
    /// it does not make a second one.
    ///
    /// The two halves belong in ONE test: "stored once" and "owed
    /// until acked" pull opposite ways, and an earlier version of
    /// this code satisfied the first by trading away the second
    /// (codex a43bd85). The ACK half runs through the real CLI
    /// surface in `events.rs`.
    #[test]
    fn an_unacked_event_survives_the_arm_and_a_replay_adds_nothing() {
        let dir = tmpdir();
        let src = src_named("s");
        let raw = r#"{"id":"1","kind":"k","content":"x"}"#;

        let (mut log, mut seen) = store(dir.path());
        let what = ingest_line(&src, &seen, raw).expect("not blank");
        assert!(apply(what, KEY, &mut log, &mut seen, |_| true));

        // A fresh handle on the same directory: the wait returned
        // without an ack, so the event is still owed — WITH its
        // payload, not as a bare marker.
        let (mut log, mut seen) = store(dir.path());
        let owed = unacked(&log);
        assert_eq!(owed.len(), 1, "an unacked event survives the arm");
        let WaitItem::CommandEvent { content, id, .. } = &owed[0] else {
            panic!("the payload is what survived")
        };
        assert_eq!((content.as_str(), id.as_str()), ("x", "1"));

        let what = ingest_line(&src, &seen, raw).expect("not blank");
        assert_eq!(what, Ingested::Duplicate);
        assert!(apply(what, KEY, &mut log, &mut seen, |_| panic!(
            "nothing to send"
        )));
        assert_eq!(unacked(&log).len(), 1, "and no second copy appears");
    }

    /// A presented event says how to finish itself, and the selector
    /// names the row it was stored as.
    #[test]
    fn a_presented_event_carries_its_ack_selector() {
        let dir = tmpdir();
        let (mut log, mut seen) = store(dir.path());
        let src = src_named("s");

        let mut sent = Vec::new();
        for n in 1..=3 {
            let raw = format!(r#"{{"id":"{n}","kind":"k"}}"#);
            let what = ingest_line(&src, &seen, &raw).expect("not blank");
            apply(what, KEY, &mut log, &mut seen, |item| {
                sent.push(item);
                true
            });
        }
        let seqs: Vec<u64> = log
            .read_rows()
            .unwrap()
            .unwrap()
            .rows
            .iter()
            .map(|r| r.seq)
            .collect();
        let got: Vec<String> = sent
            .iter()
            .map(|i| match i {
                WaitItem::CommandEvent { selector, .. } => {
                    selector.clone().expect("a logged event has a selector")
                }
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            got,
            seqs.iter()
                .map(|s| format!("{KEY}@{s}"))
                .collect::<Vec<_>>(),
            "each selector must name the row that event was stored as",
        );
        // Source-qualified, because a bare seq collides across the
        // sources sharing this directory.
        assert!(got.iter().all(|s| s.starts_with(KEY)));
    }

    /// The producer's timestamp lands in the WAL ENVELOPE.
    ///
    /// That is where every reader already looks for an event's own
    /// time — github writes it there, and the timeline reads it
    /// first. The timeline also falls back to the item's `at`, but
    /// only for rows written before this; a supervisor that stopped
    /// writing the envelope would leave every NEW row depending on a
    /// legacy path, which is why the fallback cannot be the test
    /// (codex 80e1fb9).
    #[test]
    fn a_producers_timestamp_reaches_the_wal_envelope() {
        let dir = tmpdir();
        let (mut log, mut seen) = store(dir.path());
        let src = src_named("s");

        let what = ingest_line(&src, &seen, r#"{"id":"1","content":"x","at":1730000000}"#)
            .expect("not blank");
        apply(what, KEY, &mut log, &mut seen, |_| true);
        // And one WITHOUT a time: the envelope stays empty rather
        // than inventing one.
        let what = ingest_line(&src, &seen, r#"{"id":"2","content":"y"}"#).expect("not blank");
        apply(what, KEY, &mut log, &mut seen, |_| true);

        let rows = log.read_rows().unwrap().unwrap().rows;
        let stamped: Vec<Option<u64>> = rows.iter().map(|r| r.event_at).collect();
        assert_eq!(
            stamped,
            vec![Some(1730000000), None],
            "the producer's time when it gave one, and nothing when it did not",
        );
    }

    /// An index entry that cannot be READ is damage, not absence.
    ///
    /// The three answers are distinct on purpose: an unreadable entry
    /// used to read as "seen", which suppressed the producer's only
    /// replay of an event nobody could account for. Damage is named
    /// with its path and its id, and the replay REPAIRS it.
    ///
    /// Every read here is from a FRESH `Seen`, because that is the
    /// only way damage is reachable: within an arm the warm tier
    /// answers first, and it is right to — it knows what this process
    /// ingested regardless of what the disk says.
    #[test]
    fn a_damaged_index_entry_is_named_and_repaired_by_the_replay() {
        let dir = tmpdir();
        let (mut log, mut seen) = store(dir.path());
        let src = src_named("s");
        let raw = r#"{"id":"1","kind":"k","content":"x"}"#;
        let fresh = || Seen::at(dir.path(), KEY);

        assert_eq!(seen.known("1"), Known::No, "missing");
        let what = ingest_line(&src, &seen, raw).expect("not blank");
        apply(what, KEY, &mut log, &mut seen, |_| true);
        assert_eq!(fresh().known("1"), Known::Yes, "valid, and durable");

        // Corrupt the entry: a hash collision and a truncated write
        // look the same from here, and both mean "this file does not
        // account for id 1".
        let entry = std::fs::read_dir(dir.path().join(format!("{KEY}.seen")))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(&entry, b"some other id").unwrap();
        let Known::Damaged(why) = fresh().known("1") else {
            panic!("an entry that does not hold this id is damage, not a hit")
        };
        assert!(why.contains(&entry.display().to_string()), "{why}");
        assert!(why.contains('1'), "the id must be named too: {why}");

        // A read ERROR is damage as well — not absence. A directory
        // where the entry belongs is the cheapest way to make
        // `read_to_string` fail with something that is not NotFound.
        fresh().record("2").unwrap();
        let second = std::fs::read_dir(dir.path().join(format!("{KEY}.seen")))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| std::fs::read_to_string(path).is_ok_and(|text| text == "2"))
            .unwrap();
        std::fs::remove_file(&second).unwrap();
        std::fs::create_dir(&second).unwrap();
        assert!(
            matches!(fresh().known("2"), Known::Damaged(_)),
            "an unreadable entry is damage, not `never ingested`",
        );

        // The producer's replay is accepted, which is the repair:
        // suppressing it would lose the event permanently.
        let mut arm = fresh();
        let what = ingest_line(&src, &arm, raw).expect("not blank");
        assert!(
            matches!(what, Ingested::Emit { .. }),
            "a damaged entry must not refuse the replay that repairs it",
        );
        apply(what, KEY, &mut log, &mut arm, |_| true);
        assert_eq!(fresh().known("1"), Known::Yes, "repaired");
    }

    /// `forget_all` forgets BOTH tiers.
    ///
    /// Today quarantine runs before anything is ingested, so the warm
    /// tier happens to be empty and clearing it changes nothing — a
    /// mutation that dropped the clear survived on exactly that
    /// accident. The contract is the name: a `Seen` that has
    /// forgotten everything must not still answer `Yes`, whichever
    /// tier remembers. Pinned here so the call site is free to move.
    #[test]
    fn forgetting_the_index_forgets_both_tiers() {
        let dir = tmpdir();
        let mut seen = Seen::at(dir.path(), KEY);
        seen.record("durable").unwrap();
        assert_eq!(seen.known("durable"), Known::Yes);

        seen.forget_all().unwrap();
        assert_eq!(
            seen.known("durable"),
            Known::No,
            "a forgotten id must not still be answered from memory",
        );
        assert_eq!(
            Seen::at(dir.path(), KEY).known("durable"),
            Known::No,
            "nor from disk",
        );
        // Idempotent: quarantine can happen on an arm that never
        // wrote an index at all.
        seen.forget_all().unwrap();
    }

    /// The WAL repairs the index without minting a second event.
    ///
    /// `apply` writes the log first and the index second, so a death
    /// between them leaves an id the log accounts for and the index
    /// does not. `apply` used to claim the WAL's own identity
    /// retention covered that replay; it does not, because
    /// `append_inbox` never looks at producer identities (codex
    /// 43f5ed8). Reconciling from the log at arm time is what does.
    #[test]
    fn the_log_is_authoritative_over_the_derived_index() {
        let dir = tmpdir();
        let (mut log, mut seen) = store(dir.path());
        let src = src_named("s");
        let raw = r#"{"id":"1","kind":"k","content":"x"}"#;

        let what = ingest_line(&src, &seen, raw).expect("not blank");
        apply(what, KEY, &mut log, &mut seen, |_| true);

        // THE CRASH GAP: the row landed, the index write did not.
        std::fs::remove_dir_all(dir.path().join(format!("{KEY}.seen"))).unwrap();

        let mut log = EventLog::open(dir.path(), KEY).unwrap();
        let mut arm = Seen::at(dir.path(), KEY);
        assert_eq!(arm.known("1"), Known::No, "the index has forgotten it");
        let loaded = log.load();
        assert!(
            loaded.feed_ids.iter().any(|f| f == "1"),
            "the WAL must still vouch for it, or this proves nothing",
        );
        arm.recover(loaded.feed_ids);

        assert_eq!(arm.known("1"), Known::Yes, "recovered from the log");
        assert_eq!(
            ingest_line(&src, &arm, raw).expect("not blank"),
            Ingested::Duplicate,
            "a replay must not mint a second event for an id the log already holds",
        );
        assert_eq!(unacked(&log).len(), 1, "still exactly one event");

        // And recovery REPAIRS the durable tier, so the next arm
        // needs no log at all to know it.
        assert_eq!(Seen::at(dir.path(), KEY).known("1"), Known::Yes);
    }

    /// A durable index write that FAILS still deduplicates for the
    /// rest of the arm.
    ///
    /// This is the whole reason the warm tier exists. The file tier
    /// is best-effort — `apply` reports a failed index write and
    /// carries on, because the event is already logged and refusing
    /// to present it would be the worse trade. Without the warm
    /// tier, every later replay of that id in the same arm appends
    /// another row, and a producer that replays on a timer turns one
    /// unwritable directory into an inbox full of copies.
    #[test]
    fn an_unwritable_index_still_deduplicates_within_the_arm() {
        let dir = tmpdir();
        let (mut log, mut seen) = store(dir.path());
        let src = src_named("s");
        let raw = r#"{"id":"1","kind":"k","content":"x"}"#;

        // A FILE where the index directory belongs, so `create_dir_all`
        // fails and nothing durable can be written.
        std::fs::write(dir.path().join(format!("{KEY}.seen")), b"not a directory").unwrap();
        assert!(seen.record("probe").is_err(), "the durable tier is broken");

        let what = ingest_line(&src, &seen, raw).expect("not blank");
        assert!(apply(what, KEY, &mut log, &mut seen, |_| true));
        assert_eq!(unacked(&log).len(), 1, "the event is logged and presented");

        assert_eq!(
            ingest_line(&src, &seen, raw).expect("not blank"),
            Ingested::Duplicate,
            "a replay in the same arm must not append a second row",
        );
        // And it really is the warm tier answering. A fresh `Seen`
        // over the same broken directory cannot say `Yes` — it reads
        // `Damaged` here, which is treated as not-seen, so the next
        // arm would offer the event again rather than suppress it.
        assert_ne!(
            Seen::at(dir.path(), KEY).known("1"),
            Known::Yes,
            "nothing durable was written, or this proves nothing",
        );
    }

    /// An event that cannot be LOGGED is not ingested.
    ///
    /// Not logged means not indexed, so the producer's next replay
    /// can offer it again — indexing an id whose event does not
    /// exist is how the previous version lost events.
    #[test]
    fn an_event_that_cannot_be_logged_is_not_suppressed() {
        let dir = tmpdir();
        let src = src_named("s");
        let raw = r#"{"id":"7","kind":"k","content":"x"}"#;
        let (mut log, mut seen) = store(dir.path());

        // Make the append fail: a DIRECTORY where the WAL file goes.
        std::fs::create_dir_all(dir.path().join(format!("{KEY}.jsonl"))).unwrap();
        let what = ingest_line(&src, &seen, raw).expect("not blank");
        let mut sent = false;
        apply(what, KEY, &mut log, &mut seen, |_| {
            sent = true;
            true
        });
        assert!(!sent, "nothing is presented when nothing was logged");
        assert_eq!(
            seen.known("7"),
            Known::No,
            "and the id stays unseen, so a replay can offer it again",
        );

        // Somewhere writable, it works.
        let dir = tmpdir();
        let (mut log, mut seen) = store(dir.path());
        let what = ingest_line(&src, &seen, raw).expect("not blank");
        assert!(apply(what, KEY, &mut log, &mut seen, |_| true));
        assert_eq!(seen.known("7"), Known::Yes);
        assert_eq!(unacked(&log).len(), 1);
    }

    /// ONE outstanding failure report per source.
    ///
    /// A broken source must say so, and an unacked row already wakes
    /// the agent on every arm — so a second report is a nag. Once the
    /// agent has acked, a later breakage is news again, which is why
    /// this cannot go through `Seen`: that would silence the source
    /// forever after one report.
    #[test]
    fn a_broken_source_reports_once_until_it_is_acked() {
        let dir = tmpdir();
        let (mut log, mut seen) = store(dir.path());
        let mut src = src_named("s");
        src.interrupt = true;
        let woke = std::cell::RefCell::new(Vec::new());
        let report = |log: &mut EventLog, seen: &mut Seen| {
            report_failure(&src, KEY, log, seen, "it keeps dying", |i| {
                woke.borrow_mut().push(i);
                true
            })
        };

        // An unacked ORDINARY event must not stand in for a
        // breakage report. A source that produced something, went
        // unacked, and THEN broke is exactly when the agent most
        // needs telling — and it is the state that makes a gate on
        // "any unacked row" look like it works.
        let what = ingest_line(&src, &seen, r#"{"id":"ordinary"}"#).expect("not blank");
        apply(what, KEY, &mut log, &mut seen, |_| true);
        assert!(report(&mut log, &mut seen));
        assert_eq!(
            woke.borrow().len(),
            1,
            "a breakage is news even with other work owed",
        );

        assert!(report(&mut log, &mut seen));
        assert!(report(&mut log, &mut seen));
        assert_eq!(
            woke.borrow().len(),
            1,
            "and only one report while it is still owed",
        );
        let WaitItem::CommandEvent {
            failure, selector, ..
        } = woke.borrow()[0].clone()
        else {
            panic!("expected a command event")
        };
        assert!(failure, "the wake must say the SOURCE is broken");
        assert!(
            selector.is_some(),
            "and be ackable like any other — a report the agent cannot \
             finish wakes it forever",
        );
        let failures_owed = unacked(&log)
            .iter()
            .filter(|i| matches!(i, WaitItem::CommandEvent { failure: true, .. }))
            .count();
        assert_eq!(
            failures_owed, 1,
            "the report is durable, not a passing notice",
        );

        let rows = log.read_rows().unwrap().unwrap().rows;
        assert!(rows[0].interrupt);
        assert!(
            rows.iter()
                .filter(|row| matches!(row.item, WaitItem::CommandEvent { failure: true, .. }))
                .all(|row| !row.interrupt)
        );

        // Acked: a later breakage is news again. Only the FAILURE
        // row is acked — the ordinary event stays owed, so a gate on
        // "any unacked row" would still refuse.
        let seq = log
            .read_rows()
            .unwrap()
            .unwrap()
            .rows
            .iter()
            .find(|r| matches!(r.item, WaitItem::CommandEvent { failure: true, .. }))
            .expect("the failure row")
            .seq;
        log.append_ack(seq).unwrap();
        assert!(report(&mut log, &mut seen));
        assert_eq!(
            woke.borrow().len(),
            2,
            "a source that breaks again must be able to say so",
        );
    }

    /// A producer cannot report itself broken — the kind is clank's.
    #[test]
    fn a_producer_cannot_forge_a_failure() {
        let dir = tmpdir();
        let (_, seen) = store(dir.path());
        let src = src_named("s");
        let raw = format!(r#"{{"id":"1","kind":"{FAILURE_KIND}"}}"#);
        let Some(Ingested::Refuse(why)) = ingest_line(&src, &seen, &raw) else {
            panic!("a forged failure must be refused")
        };
        assert!(why.contains(FAILURE_KIND), "{why}");
    }

    /// The id is hashed into the filename, so arbitrary producer text
    /// is a valid identity.
    #[test]
    fn an_id_may_be_any_text_a_producer_likes() {
        let dir = tmpdir();
        let mut seen = Seen::at(dir.path(), KEY);
        for id in [
            "a/b/c",
            "../escape",
            "with space",
            &"x".repeat(4096),
            "CASE",
            "case",
            "",
        ] {
            assert_eq!(seen.known(id), Known::No, "{id:?} starts unseen");
            seen.record(id).unwrap();
            assert_eq!(seen.known(id), Known::Yes, "{id:?} is recorded");
        }
        // Case matters even where the filesystem folds it.
        assert_eq!(seen.known("CASE"), Known::Yes);
        assert_eq!(seen.known("case"), Known::Yes);
    }

    /// Only `id` is required now that nothing filters.
    #[test]
    fn a_line_needs_only_an_id() {
        let src = src_named("deploys");
        let dir = tmpdir();
        let seen = Seen::at(dir.path(), KEY);

        let what = ingest_line(&src, &seen, r#"{"id":"1"}"#).expect("not blank");
        let Ingested::Emit { item, .. } = what else {
            panic!("a bare id is an event")
        };
        let WaitItem::CommandEvent { event, content, .. } = *item else {
            panic!("expected a command event")
        };
        assert_eq!(event, "deploys", "kind defaults to the source name");
        assert_eq!(content, "", "and an absent content still renders");

        assert_eq!(
            parse_line(r#"{"kind":"deploy"}"#).unwrap().unwrap_err(),
            Refused::NoId,
        );
        assert_eq!(
            parse_line(r#"{"id":"  ","kind":"k"}"#)
                .unwrap()
                .unwrap_err(),
            Refused::NoId,
        );
        assert!(matches!(
            parse_line("not json").unwrap().unwrap_err(),
            Refused::Malformed(_),
        ));
        assert!(parse_line("   ").is_none(), "a blank line is not an error");
    }

    /// The stored body is the WHOLE message, minus terminal control.
    ///
    /// This test used to assert the opposite — `!content.contains
    /// ("second")` — because ingestion ran the row preview `one_line`
    /// and kept only the first line. The event was stored already cut,
    /// so no later surface could show a second paragraph however it
    /// rendered. The half of the old test that was a real contract
    /// stays: no escape reaches the body, and now not even its syntax.
    #[test]
    fn content_keeps_its_lines_and_loses_only_terminal_control() {
        let src = src_named("s");
        let dir = tmpdir();
        let seen = Seen::at(dir.path(), KEY);
        let what = ingest_line(
            &src,
            &seen,
            r#"{"id":"1","kind":"k","content":"first \u001b[31mred\u001b[0m\n\n  indented\tcol\r\nüñí 中文 ✓\u0007\u001b]0;title\u0007end"}"#,
        )
        .expect("not blank");
        let Ingested::Emit { item, .. } = what else {
            panic!("expected an emit")
        };
        let WaitItem::CommandEvent { content, .. } = *item else {
            panic!("expected a command event")
        };
        assert_eq!(
            content, "first red\n\n  indented\tcol\nüñí 中文 ✓end",
            "every line, the blank one, the indent, the tab and the Unicode survive; \
             the colour, the bell and the title sequence go whole, and CRLF is a newline"
        );
    }

    /// The backend's directory is its own, distinct per source, and a
    /// CHILD of `sources/`.
    ///
    /// It used to be a slug of the name, which collided the same way
    /// the key did — and worse: a source called `..` produced
    /// `sources/..`, the agent directory itself (codex 1948041).
    #[test]
    fn the_backend_state_dir_is_distinct_and_contained() {
        let agent = std::path::Path::new("/a/.clank/agents/claude");
        let at = |n: &str| backend_state_dir(agent, &src_named(n).source_key());

        // Names that slug alike still get their own directory.
        assert_ne!(at("ci/deploy"), at("ci-deploy"));
        assert_ne!(at("a b"), at("a-b"));
        // Stable for the same name.
        assert_eq!(at("deploys"), at("deploys"));

        // Always ONE component under `sources/`, so nothing escapes.
        for name in ["..", ".", "../..", "a/../../b", "deploys"] {
            let d = at(name);
            let rest: Vec<_> = d
                .strip_prefix(agent.join("sources"))
                .unwrap()
                .components()
                .collect();
            assert_eq!(rest.len(), 1, "`{name}` must be one component: {d:?}");
            assert!(
                d.starts_with(agent.join("sources")),
                "`{name}` escaped: {d:?}",
            );
            assert!(
                !d.components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
                "`{name}` yielded a parent component: {d:?}",
            );
        }
    }

    /// Backoff is prompt, doubles, and caps.
    #[test]
    fn backoff_is_prompt_then_doubles_then_caps() {
        assert_eq!(backoff(1), BACKOFF_FIRST, "the first restart is quick");
        assert_eq!(backoff(2), BACKOFF_FIRST * 2);
        assert_eq!(backoff(3), BACKOFF_FIRST * 4);
        assert_eq!(backoff(30), BACKOFF_CAP, "and never grows past the cap");
        let mut last = std::time::Duration::ZERO;
        for n in 1..1000 {
            let d = backoff(n);
            assert!(d >= last, "backoff must not go backwards at {n}");
            assert!(d <= BACKOFF_CAP);
            last = d;
        }
    }

    #[test]
    fn icons_are_visible_graphemes_not_single_column_scalars() {
        for glyph in ["*", "→", "💬", "💤", "☁️", "e\u{0301}", "👩‍💻", "🇦🇺", "1️⃣"]
        {
            assert_eq!(display_glyph(glyph), Some(glyph), "{glyph:?}");
        }
    }

    #[test]
    fn unsafe_or_non_renderable_icons_use_the_default_badge() {
        for glyph in [
            "",
            "ab",
            " ",
            "\n",
            "\t",
            "\u{1b}",
            "x\n",
            "\u{0301}",
            "\u{200D}",
            "\u{FE0F}",
            "\u{05B0}",
            "\u{061C}",
            "\u{FE20}",
            "\u{E0100}",
            "\u{0F71}",
            "\u{1AB0}",
            "\u{2064}",
            "\u{202E}",
        ] {
            assert_eq!(display_glyph(glyph), None, "{glyph:?}");
        }
    }
}

/// What a host recorded about the process group its producer runs in,
/// so a successor can RECOGNISE it before signalling anything.
///
/// The host's own retirement covers every end it lives through. It
/// does not cover SIGKILL: nothing runs, the group is reparented to
/// init, and the user's command goes on running. The next owner of
/// this source is the only thing in a position to notice, and it can
/// only notice what was written down.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "OnDisk", into = "OnDisk")]
pub(crate) enum Producer {
    /// Led by a clank anchor (`source_group`) that outlives every
    /// other member, identified by a birth the kernel reports as
    /// numbers — so no reader's `TZ` or locale can make the same
    /// process look like another.
    Anchored {
        pgid: i32,
        birth: crate::proc_identity::ProcToken,
    },
    /// Written before records were versioned: the user's command led
    /// its own group, identified by `ps -o lstart` as rendered under
    /// whatever `TZ` and locale THAT process had. Nobody wrote that
    /// environment down, so a mismatch here proves nothing about
    /// reuse — the same process reads `08:43:52` under `TZ=UTC` and
    /// `19:43:52` under `TZ=Australia/Sydney`.
    Legacy { pgid: i32, started: String },
}

impl Producer {
    pub(crate) fn pgid(&self) -> i32 {
        match self {
            Producer::Anchored { pgid, .. } | Producer::Legacy { pgid, .. } => *pgid,
        }
    }
}

/// The version an anchored record carries. Legacy records carry none.
const ANCHORED: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
struct OnDisk {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<u32>,
    pgid: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    started: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    birth: Option<crate::proc_identity::ProcToken>,
}

impl TryFrom<OnDisk> for Producer {
    type Error = String;

    fn try_from(raw: OnDisk) -> Result<Self, String> {
        match raw {
            OnDisk {
                version: Some(ANCHORED),
                pgid,
                started: None,
                birth: Some(birth),
            } => Ok(Producer::Anchored { pgid, birth }),
            OnDisk {
                version: None,
                pgid,
                started: Some(started),
                birth: None,
            } => Ok(Producer::Legacy { pgid, started }),
            OnDisk {
                version: Some(v), ..
            } if v != ANCHORED => Err(format!("record version {v} is not one this clank reads")),
            _ => Err("neither an anchored nor a legacy producer record".into()),
        }
    }
}

impl From<Producer> for OnDisk {
    fn from(record: Producer) -> Self {
        match record {
            Producer::Anchored { pgid, birth } => OnDisk {
                version: Some(ANCHORED),
                pgid,
                started: None,
                birth: Some(birth),
            },
            Producer::Legacy { pgid, started } => OnDisk {
                version: None,
                pgid,
                started: Some(started),
                birth: None,
            },
        }
    }
}

/// What `ps` says about a live process, for a legacy record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Observed {
    pub(crate) pgid: i32,
    pub(crate) started: String,
}

/// What looking for a pid found.
///
/// `Gone` and `Unknown` must not be the same answer: a pid that has
/// provably exited makes its record spent, while a look that FAILED
/// says nothing at all — and forgetting the record on a failed look
/// discards the only evidence that an orphan exists (codex on
/// 697807a).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Look {
    Alive(Observed),
    Gone,
    Unknown,
}

/// Whether anything remains in a recorded process group.
///
/// The GROUP is what a producer owns, what gets signalled, and
/// therefore what has to be empty before a replacement may start. The
/// leader's absence does not answer this: a `sh -c` that backgrounds
/// something exits while that descendant keeps running in the same
/// group, and a check that looked only at the leader would call the
/// record spent, skip the escalation, and start a second producer
/// beside the survivor (codex on 14ab92e).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Group {
    Empty,
    Occupied,
    Unknown,
}

/// Whether a recorded group's leader is the process that was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Leader {
    /// The recorded incarnation, still leading the recorded group.
    Same,
    /// A different incarnation holds the pid, by an identity no
    /// environment can vary. The recorded one has ended.
    Different,
    /// A legacy record's rendering does not match the live process.
    /// NOT `Different`: the rendering depends on an environment nobody
    /// recorded, so this is as consistent with our own producer as
    /// with a stranger.
    Unmatched,
    Gone,
    Unknown,
}

/// Whether a live process is the same INCARNATION a legacy record
/// names.
///
/// `argv[0]` was tried and is wrong in both directions — a recycled
/// pid running another copy of the same command matches, and the
/// ordinary `sh -c '<cmd>'` form EXECS, so the live process reports
/// the exec'd name and the producer stops recognising its own child
/// (codex on 697807a). A start time survives exec and separates
/// incarnations.
pub(crate) fn same_incarnation(pgid: i32, started: &str, seen: &Observed) -> bool {
    !started.is_empty() && seen.pgid == pgid && seen.started == started
}

/// A legacy record's leader, judged only by the rendering this
/// process's own environment produces.
///
/// Other environments are never tried. A guessed `TZ` is not the
/// recorder's, and searching widens the dangerous matches rather than
/// recovering lost provenance: a Sydney record of an 08:45 UTC start
/// reads `19:45`, and a stranger that took the pid at 19:45 UTC reads
/// exactly that in UTC (codex on fd20615).
pub(crate) fn legacy_leader(pgid: i32, started: &str, look: Look) -> Leader {
    match look {
        Look::Gone => Leader::Gone,
        Look::Unknown => Leader::Unknown,
        Look::Alive(seen) if same_incarnation(pgid, started, &seen) => Leader::Same,
        Look::Alive(_) => Leader::Unmatched,
    }
}

/// An anchored record's leader, from an inspection against its birth.
pub(crate) fn anchored_leader(
    pgid: i32,
    seen: crate::proc_identity::inspect::Inspection,
) -> Leader {
    use crate::proc_identity::inspect::Inspection;
    match seen {
        Inspection::Verified(details) if details.group == Ok(pgid) => Leader::Same,
        // Our anchor never changes group, so this is not a state the
        // record describes.
        Inspection::Verified(_) => Leader::Unknown,
        Inspection::Mismatched => Leader::Different,
        Inspection::Ended => Leader::Gone,
        Inspection::Unavailable(_) => Leader::Unknown,
    }
}

/// The recorded group's leader, as this system reports it.
pub(crate) fn leader_of(record: &Producer) -> Leader {
    match record {
        Producer::Anchored { pgid, birth } => {
            anchored_leader(*pgid, crate::proc_identity::inspect::inspect(*pgid, birth))
        }
        Producer::Legacy { pgid, started } => legacy_leader(*pgid, started, look(*pgid)),
    }
}

/// The group producing for the source whose record is in `store_dir`:
/// the recorded anchor, while this system says it is still the process
/// recorded. Only reads: it takes no lease and retires nothing. A legacy
/// record names its process by a rendering nobody can check, so it
/// proves nothing here.
pub(crate) fn verified_producer(store_dir: &std::path::Path, key: &str) -> Option<i32> {
    let body = std::fs::read(producer_path(store_dir, key)).ok()?;
    match serde_json::from_slice::<Producer>(&body).ok()? {
        Producer::Anchored { pgid, birth } => {
            let seen = crate::proc_identity::inspect::inspect(pgid, &birth);
            (anchored_leader(pgid, seen) == Leader::Same).then_some(pgid)
        }
        Producer::Legacy { .. } => None,
    }
}

/// Where a source records its producer, beside that source's log.
pub(crate) fn producer_path(store_dir: &std::path::Path, key: &str) -> std::path::PathBuf {
    store_dir.join(format!("{key}.producer"))
}

/// Note that the group `pgid` leads is this source's producer, for a
/// successor to find.
///
/// Written through a temp file and renamed, because this is RECOVERY
/// evidence: a truncating write interrupted by the very SIGKILL this
/// exists to survive would leave a record that parses as nothing.
pub(crate) fn record_producer(
    store_dir: &std::path::Path,
    key: &str,
    record: &Producer,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let path = producer_path(store_dir, key);
    let tmp = path.with_extension("producer.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)
}

/// Ask `ps` about one pid, for a legacy record.
///
/// Death is settled by `kill(pid, 0)` and never by `ps`: a failed
/// query is not evidence of absence, and on this platform even a
/// missing pid writes to stderr, so no amount of reading `ps`'s
/// output distinguishes "gone" from "that question did not work"
/// (codex on 8d3d669).
///
/// The environment is deliberately NOT pinned. A legacy record was
/// rendered under its recorder's environment, so the only comparison
/// that is not a guess is the one the record's own era made: this
/// process's rendering against the stored string.
pub(crate) fn look(pid: i32) -> Look {
    // `Theirs` is not `Gone`: a pid that exists and is somebody else's
    // must not be read as a spent record, and `ps` below is what says
    // whether it is the incarnation we recorded.
    if crate::signal::probe_process(pid) == crate::signal::Probe::Gone {
        return Look::Gone;
    }
    let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "pgid=,lstart=", "-p"])
        .arg(pid.to_string())
        .output()
    else {
        return Look::Unknown;
    };
    if !out.status.success() {
        return Look::Unknown;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.split_whitespace();
    let Some(Ok(pgid)) = parts.next().map(str::parse) else {
        return Look::Unknown;
    };
    let started = parts.collect::<Vec<_>>().join(" ");
    if started.is_empty() {
        return Look::Unknown;
    }
    Look::Alive(Observed { pgid, started })
}

/// Whether a process group still has members.
///
/// `kill(-pgid, 0)` asks the kernel directly, which is the group-level
/// counterpart of the `ESRCH` check above and needs no enumeration.
pub(crate) fn group_of(pgid: crate::signal::Pgid) -> Group {
    group_from(crate::signal::probe_group(pgid))
}

/// Read a group probe as occupancy.
///
/// A group that is somebody ELSE's counts as occupied: that it exists
/// at all is every reason not to call it empty, and whether we may end
/// it is a separate question with a separate answer.
fn group_from(probe: crate::signal::Probe) -> Group {
    use crate::signal::Probe;
    match probe {
        Probe::Ours | Probe::Theirs => Group::Occupied,
        Probe::Gone => Group::Empty,
        Probe::Unknown(_) => Group::Unknown,
    }
}

/// What the recorded group IS, from its leader and its occupancy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Standing {
    /// Nothing remains. The record is spent.
    Gone,
    /// Still ours, and still ours to end.
    Ours,
    /// The id names another incarnation now, proven by an identity no
    /// environment varies — which means ours ENDED.
    Theirs,
    /// Members remain and there is no leader to identify them by.
    ///
    /// NOT a synonym for ours. The kernel only promises that an id is
    /// not reused while that group still has members; it says nothing
    /// about whether THIS group has been alive continuously since the
    /// record was written. So these members may be a stranger's: our
    /// group ended, the number was later re-issued, and that group's
    /// leader exited too, leaving its own descendants (codex on
    /// aef11a3). Signalling them would be 49dec39's catastrophe in
    /// miniature, so this earns no signal — and, because it is equally
    /// no evidence that our producer has ended, no replacement either.
    Unidentified,
    /// A live leader a legacy record cannot be matched against. As
    /// consistent with ours as with a stranger, so it earns neither a
    /// signal nor a replacement.
    Unrecognised,
    /// Could not be established.
    Unsure,
}

/// Decide a recorded group's standing.
///
/// The LEADER is the only thing that identifies a group, because its
/// incarnation is the only part of a group that was ever written down.
/// The kernel's guarantee — an id is not reused while its group still
/// has members — is what makes that identity meaningful, but it does
/// NOT bridge a gap in observation. A record read hours later cannot
/// tell "our group, leader exited, descendants remain" from "our group
/// ended, the number was re-issued, that group's leader exited too,
/// its descendants remain". Both read as a missing leader over an
/// occupied group, and only one of them is ours to kill. An anchored
/// group never reaches that question: its anchor is killed last.
pub(crate) fn standing(leader: &Leader, group: &Group) -> Standing {
    match (leader, group) {
        (Leader::Unknown, _) | (_, Group::Unknown) => Standing::Unsure,
        (Leader::Different, _) => Standing::Theirs,
        (Leader::Unmatched, _) => Standing::Unrecognised,
        (Leader::Same, Group::Occupied) => Standing::Ours,
        // A live leader over an empty group cannot both be true.
        (Leader::Same, Group::Empty) => Standing::Unsure,
        (Leader::Gone, Group::Empty) => Standing::Gone,
        (Leader::Gone, Group::Occupied) => Standing::Unidentified,
    }
}

/// Whether a source may start a producer yet.
///
/// A RESULT, not a courtesy. The whole point of retiring the previous
/// owner's group is that a second producer must not run beside it, so
/// the outcome has to reach the decision it exists to gate — and
/// `record_producer` would overwrite the only evidence of the first
/// one on its way past (codex on 8d3d669).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Retired {
    /// The recorded group is empty and its record is gone. Launch.
    Clear,
    /// It may still have members, or its state could not be
    /// established. Its record is kept, the backlog keeps being
    /// presented, and the launch waits for a later look.
    Deferred(String),
}

/// The system calls a retirement makes, so the whole decision is
/// reachable without a process to kill — a test that signalled a real
/// group would be a test that can kill this machine's own processes
/// whenever the decision under test is wrong.
pub(crate) trait Os {
    fn leader(&self, record: &Producer) -> Leader;
    fn group(&self, pgid: crate::signal::Pgid) -> Group;
    fn signal(&self, pgid: crate::signal::Pgid, sig: libc::c_int);
    /// Collect any of the group's members that are THIS process's
    /// children. An exited child nobody has waited for still occupies
    /// its group, so a host verifying its own group empty has to reap
    /// first; for an inherited group, init already does.
    fn collect(&self);
    fn pause(&self, d: std::time::Duration);
    fn now(&self) -> std::time::Instant;
}

/// The real system, collecting the given children.
pub(crate) struct Native<'a> {
    pub(crate) children: &'a [crate::signal::Pid],
}

impl Os for Native<'_> {
    fn leader(&self, record: &Producer) -> Leader {
        leader_of(record)
    }
    fn group(&self, pgid: crate::signal::Pgid) -> Group {
        group_of(pgid)
    }
    fn signal(&self, pgid: crate::signal::Pgid, sig: libc::c_int) {
        crate::signal::signal_group(pgid, sig)
    }
    fn collect(&self) {
        for child in self.children {
            crate::signal::collect_exited_child(*child);
        }
    }
    fn pause(&self, d: std::time::Duration) {
        std::thread::sleep(d)
    }
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
}

/// How long a group is given to end on a `SIGTERM` before `SIGKILL`.
pub(crate) const ESCALATE_AFTER: std::time::Duration = std::time::Duration::from_millis(200);

/// How often, and how many times, a retirement looks again for a
/// group in transition to settle.
const SETTLE_STEP: std::time::Duration = std::time::Duration::from_millis(10);
const SETTLE_LOOKS: u32 = 40;

/// How long an identification stays good enough to escalate on when
/// the leader has since gone.
///
/// Generous against the `ESCALATE_AFTER` sleep and tight against pid
/// reuse, which needs the allocator to walk the whole pid space. The
/// measurement is what matters, not the number: a process that was
/// stopped or starved resumes with the sleep "finished" and an
/// arbitrary amount of the world's time elapsed, and only a clock
/// notices (codex on 1d90b9b).
pub(crate) const IDENTIFICATION_KEEPS: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether an escalation may still be sent.
///
/// `sleep` gives a MINIMUM delay and never a maximum gap in
/// observation. Two things therefore have to hold: the leader must not
/// have been replaced by a different live one, and the identification
/// must not have aged out while this process was not running.
pub(crate) fn may_escalate(
    seen: &Standing,
    since_identified: std::time::Duration,
    keeps: std::time::Duration,
) -> bool {
    match seen {
        // The same leader is still there, so the identity is current
        // rather than remembered. Nothing needs to be assumed.
        Standing::Ours => true,
        // The leader went while members remain. Ours only if little
        // enough of the world has happened for the id to still be
        // ours — which is the claim a clock can check and a sleep
        // cannot.
        Standing::Unidentified => since_identified <= keeps,
        Standing::Theirs | Standing::Gone | Standing::Unrecognised | Standing::Unsure => false,
    }
}

/// Whose group a retirement is ending, for what it reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Whose {
    /// A previous host's, found through its record.
    Inherited,
    /// This process's own.
    Own,
}

/// End a recorded group: TERM, a grace, KILL, and confirmation that
/// it is empty — once, bounded.
///
/// The record at `path` is removed ONLY once the group is confirmed
/// empty (or proven to be another incarnation's). Every exit that
/// stops short of that leaves it in place, so a host that dies at any
/// point in here leaves its successor something to finish with.
pub(crate) fn retire(
    path: &std::path::Path,
    record: &Producer,
    os: &impl Os,
    whose: Whose,
) -> Retired {
    // Validated BEFORE anything is looked at or signalled, and the
    // validation is what the rest of this function then carries: a
    // record naming one of the special ids cannot have come from
    // `record_producer` — a child's pid is never 0 or 1 — so it is
    // evidence from somewhere else, and nothing here will act on it.
    let Some(pgid) = crate::signal::Pgid::new(record.pgid()) else {
        return Retired::Deferred(format!(
            "{} names pgid {}, which is not a process group; remove it",
            path.display(),
            record.pgid()
        ));
    };

    let now = || standing(&os.leader(record), &os.group(pgid));
    // Bounded, and spent once per question: this settles a transition,
    // it does not watch a group.
    let settled = |until: fn(&Standing) -> bool| {
        os.collect();
        let mut seen = now();
        for _ in 0..SETTLE_LOOKS {
            if until(&seen) {
                break;
            }
            os.pause(SETTLE_STEP);
            os.collect();
            seen = now();
        }
        seen
    };
    let spent = || {
        let _ = std::fs::remove_file(path);
        Retired::Clear
    };

    // A child of ours that has just exited is still a member until it
    // is reaped, and reads meanwhile as a group with no leader — so
    // somebody else's. That is a moment, not an answer.
    match settled(|s| !matches!(s, Standing::Unidentified | Standing::Unsure)) {
        Standing::Gone | Standing::Theirs => return spent(),
        Standing::Unsure => {
            return Retired::Deferred(format!(
                "cannot tell what has become of the producer group (pgid {pgid})"
            ));
        }
        Standing::Unidentified => {
            return Retired::Deferred(format!(
                "pgid {pgid} has members but no leader to identify them by, so they may not \
                 be this source's — neither signalling them nor replacing them is safe; \
                 remove {} once you have established what they are",
                path.display()
            ));
        }
        Standing::Unrecognised => {
            return Retired::Deferred(format!(
                "{} was written by an earlier clank, and its start time does not match the \
                 live pid {pgid} under this environment — that may be the same producer \
                 rendered under another TZ or locale, so neither signalling it nor \
                 replacing it is safe; remove the record once you have established what \
                 pid {pgid} is",
                path.display()
            ));
        }
        Standing::Ours => {}
    }

    // IDENTIFIED, and the moment of it is recorded rather than assumed.
    let identified_at = os.now();
    if whose == Whose::Inherited {
        eprintln!("wait: a command source left a producer behind (pgid {pgid}); ending it");
    }
    os.signal(pgid, libc::SIGTERM);
    os.pause(ESCALATE_AFTER);
    os.collect();

    // Looked at AGAIN, leader included: the term may have ended the
    // leader while a descendant ignored it, and it may equally have
    // ended the whole group and let the id be re-issued. Occupancy
    // alone cannot tell those apart, and the gap is MEASURED rather
    // than assumed to be the sleep.
    if may_escalate(
        &now(),
        os.now().saturating_duration_since(identified_at),
        IDENTIFICATION_KEEPS,
    ) {
        os.signal(pgid, libc::SIGKILL);
    }

    match settled(|s| matches!(s, Standing::Gone | Standing::Theirs)) {
        Standing::Gone | Standing::Theirs => spent(),
        Standing::Ours | Standing::Unidentified | Standing::Unrecognised => {
            Retired::Deferred(format!("the producer group (pgid {pgid}) is still running"))
        }
        Standing::Unsure => Retired::Deferred(format!(
            "cannot tell whether the producer group (pgid {pgid}) has ended"
        )),
    }
}

/// End a previous owner's producer group, if one is still running.
///
/// Called by whoever has just taken this source's ingest lease, which
/// means the previous owner is gone — a lease is held for the life of
/// its process. What may survive it is the process GROUP, and starting
/// a second producer beside it is how a SIGKILLed monitor's command
/// sources accumulate.
///
/// The LEASE is the entitlement to signal, not the orphan's parentage.
/// An earlier version demanded `ppid == 1`, which is what an orphan
/// looks like here but NOT on a Linux session with a subreaper, where
/// orphans reparent to the session manager instead.
pub(crate) fn reap_previous_producer(store_dir: &std::path::Path, key: &str) -> Retired {
    reap_previous_producer_with(store_dir, key, &Native { children: &[] })
}

/// [`reap_previous_producer`] with the system calls supplied.
pub(crate) fn reap_previous_producer_with(
    store_dir: &std::path::Path,
    key: &str,
    os: &impl Os,
) -> Retired {
    let path = producer_path(store_dir, key);
    let record = match std::fs::read(&path) {
        Ok(body) => match serde_json::from_slice::<Producer>(&body) {
            Ok(record) => record,
            // Evidence that cannot be read is UNKNOWN prior state, not
            // absence, and a launch would overwrite it.
            Err(e) => {
                return Retired::Deferred(format!(
                    "{} is not a usable producer record ({e}); remove it if no producer \
                     is running",
                    path.display()
                ));
            }
        },
        // The ordinary first launch: nothing was ever recorded.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Retired::Clear,
        Err(e) => {
            return Retired::Deferred(format!("cannot read {} ({e})", path.display()));
        }
    };
    retire(&path, &record, os, Whose::Inherited)
}

#[cfg(test)]
mod producer_tests {
    use super::*;
    use crate::signal::Pgid;
    use std::os::unix::process::CommandExt;
    use std::time::Duration;

    const WHEN: &str = "Thu 1 Oct 16:58:08 2026";

    fn birth() -> crate::proc_identity::ProcToken {
        crate::proc_identity::ProcToken::MacStart { sec: 7, usec: 8 }
    }

    /// An anchored record of group 4242, as a host writes one.
    fn recorded(dir: &std::path::Path) {
        record_producer(
            dir,
            "src-1",
            &Producer::Anchored {
                pgid: 4242,
                birth: birth(),
            },
        )
        .unwrap();
    }

    /// The bytes an earlier clank wrote, before records were versioned.
    fn legacy_bytes(pgid: i32, started: &str) -> Vec<u8> {
        format!(r#"{{"pgid":{pgid},"started":"{started}"}}"#).into_bytes()
    }

    fn alive() -> Observed {
        Observed {
            pgid: 4242,
            started: WHEN.into(),
        }
    }

    /// One logged row, with only the fields this decision reads.
    fn row(seq: u64, ident: &str, acked: bool) -> crate::cli::github::log::EventRow {
        crate::cli::github::log::EventRow {
            replies: Vec::new(),
            reply_history_pruned: false,
            interrupt: false,
            interrupt_hint: None,
            interrupt_attempt: None,
            seq,
            at: 0,
            event_at: None,
            transport: crate::cli::github::log::Transport::Command,
            woken_at: None,
            acked,
            reply: None,
            reply_accepted: false,
            baseline: false,
            ident: ident.into(),
            feed_id: None,
            action_key: None,
            item: clank_core::wait::WaitItem::CommandEvent {
                presentation: None,
                source_cwd: None,
                name: "deploys".into(),
                id: ident.into(),
                event: "deploys".into(),
                content: ident.into(),
                at: None,
                failure: false,
                requires_reply: false,
                reply_route: None,
                selector: None,
            },
        }
    }

    fn selectors(items: &[clank_core::wait::WaitItem]) -> Vec<String> {
        items
            .iter()
            .map(|i| match i {
                clank_core::wait::WaitItem::CommandEvent { selector, .. } => {
                    selector.clone().unwrap_or_default()
                }
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// A client admitted LATER is owed the backlog, and a second client
    /// is owed it too.
    ///
    /// The failure this excludes is the monitor keeping one `shown` set
    /// for itself: the first client would drain the backlog and every
    /// client after it would be told nothing about rows logged before
    /// it arrived — which is every agent that reconnects.
    #[test]
    fn each_client_is_owed_the_unacked_backlog_once() {
        let logged = || vec![row(1, "a", false), row(2, "b", true), row(3, "c", false)];

        let mut first = std::collections::HashSet::new();
        let owed = owed_from_log(logged(), "deploys", None, &mut first);
        assert_eq!(
            selectors(&owed),
            vec!["deploys@1", "deploys@3"],
            "unacked only, and each stamped with the selector its ack needs"
        );

        // The same client again: nothing, because it has been shown
        // them and only an ACK retires a row.
        assert!(owed_from_log(logged(), "deploys", None, &mut first).is_empty());

        // A client admitted later, with a set of its own: owed the same
        // backlog. Being shown to somebody else is not being handled.
        let mut second = std::collections::HashSet::new();
        assert_eq!(
            selectors(&owed_from_log(logged(), "deploys", None, &mut second)),
            vec!["deploys@1", "deploys@3"],
        );

        // And once the row IS acked, nobody is owed it — not even a
        // client that never saw it.
        let acked = vec![row(1, "a", true), row(2, "b", true), row(3, "c", true)];
        let mut third = std::collections::HashSet::new();
        assert!(owed_from_log(acked, "deploys", None, &mut third).is_empty());
    }

    /// A github row replayed from the log gets its instructions back.
    ///
    /// The WAL stores none — the watch's prompt is stamped at
    /// presentation so that editing the watch retitles events already
    /// logged — so a backlog handed straight to an agent would arrive
    /// with the operator's instructions missing.
    #[test]
    fn a_replayed_github_row_is_stamped_with_the_watchs_prompt() {
        let github = |seq: u64| crate::cli::github::log::EventRow {
            woken_at: None,
            replies: Vec::new(),
            reply_history_pruned: false,
            interrupt: false,
            interrupt_hint: None,
            interrupt_attempt: None,
            seq,
            at: 0,
            event_at: None,
            transport: crate::cli::github::log::Transport::Poll,
            acked: false,
            reply: None,
            reply_accepted: false,
            baseline: false,
            ident: format!("i{seq}"),
            feed_id: None,
            action_key: None,
            item: clank_core::wait::WaitItem::GithubEvent {
                presentation: None,
                repo: "o/n".into(),
                event: "pr_opened".into(),
                detail: None,
                number: Some(7),
                title: None,
                actor: None,
                url: None,
                // As the log holds it: never stored, so never read back.
                instructions: None,
                content: None,
            },
        };
        let instructions = |items: &[clank_core::wait::WaitItem]| -> Vec<Option<String>> {
            items
                .iter()
                .map(|i| match i {
                    clank_core::wait::WaitItem::GithubEvent { instructions, .. } => {
                        instructions.clone()
                    }
                    _ => None,
                })
                .collect()
        };

        let mut shown = std::collections::HashSet::new();
        assert_eq!(
            instructions(&owed_from_log(
                vec![github(1)],
                "prs",
                Some("triage and reply"),
                &mut shown
            )),
            vec![Some("triage and reply".to_string())],
        );

        // And a promptless watch stamps nothing rather than something
        // empty, which is what keeps its WAL bytes byte-stable.
        let mut other = std::collections::HashSet::new();
        assert_eq!(
            instructions(&owed_from_log(vec![github(1)], "prs", None, &mut other)),
            vec![None],
        );
    }

    /// Two sources both numbering from 1 do not suppress each other.
    ///
    /// A `(seq, ident)` pair belongs to ONE log. A client with several
    /// sources keeps one set, so the source has to be part of the
    /// identity — otherwise the second source's first row looks like
    /// the first source's first row and is silently never delivered.
    #[test]
    fn one_set_per_client_still_separates_its_sources() {
        let mut shown = std::collections::HashSet::new();
        assert_eq!(
            selectors(&owed_from_log(
                vec![row(1, "a", false)],
                "deploys",
                None,
                &mut shown
            )),
            vec!["deploys@1"]
        );
        assert_eq!(
            selectors(&owed_from_log(
                vec![row(1, "a", false)],
                "builds",
                None,
                &mut shown
            )),
            vec!["builds@1"],
            "the same seq and ident in ANOTHER log is another row"
        );
        // And within one source it is still suppressed.
        assert!(owed_from_log(vec![row(1, "a", false)], "deploys", None, &mut shown).is_empty());
    }

    /// A row that reappears under the same seq and ident is the same
    /// row, not a second one.
    #[test]
    fn a_replayed_row_is_not_owed_twice() {
        let mut shown = std::collections::HashSet::new();
        assert_eq!(
            owed_from_log(vec![row(7, "x", false)], "d", None, &mut shown).len(),
            1
        );
        assert!(owed_from_log(vec![row(7, "x", false)], "d", None, &mut shown).is_empty());
        // A different ident at the same seq is a different row, which
        // is why the pair and not the seq is the key.
        assert_eq!(
            owed_from_log(vec![row(7, "y", false)], "d", None, &mut shown).len(),
            1
        );
    }

    /// A `--event` source does not share a namespace with a configured
    /// one, nor with another invocation of the same name.
    ///
    /// `CommandSource::source_key` is derived from the NAME alone, so
    /// a CLI `deploys` and a configured `deploys` would own the same
    /// log and the same ingest lease. With a monitor permanently
    /// holding that lease, the invocation's task tails the monitor's
    /// log and never runs the command it was given — and two
    /// invocations of one name for one author would receive each
    /// other's events (codex on a135276).
    #[test]
    fn an_invocations_source_owns_its_own_namespace() {
        use crate::cli::event_source::SourceOrigin;
        let named = "command-deploys-1234abcd".to_string();

        assert_eq!(
            storage_key(named.clone(), SourceOrigin::Config, "whatever"),
            named,
            "an author's configured source is shared by every reader of \
             that author, which is what lets one hold and the rest tail"
        );

        let one = storage_key(named.clone(), SourceOrigin::Cli, "inv-1");
        let two = storage_key(named.clone(), SourceOrigin::Cli, "inv-2");
        assert_ne!(one, named, "and an invocation's is not the configured one");
        assert_ne!(one, two, "nor another invocation's of the same name");
        assert_eq!(
            one,
            storage_key(named, SourceOrigin::Cli, "inv-1"),
            "while being stable for the invocation that owns it, so its \
             own rows stay ackable and discoverable"
        );
    }

    #[test]
    fn identity_is_the_incarnation_not_the_name() {
        assert!(same_incarnation(4242, WHEN, &alive()));

        // The case that made a NAME wrong in the permissive direction:
        // a pid re-issued to another copy of the same command. There
        // is no name here to be fooled by, and the start time differs.
        assert!(
            !same_incarnation(
                4242,
                WHEN,
                &Observed {
                    started: "Thu 1 Oct 17:04:00 2026".into(),
                    ..alive()
                }
            ),
            "a recycled pid is a different incarnation, same command or not"
        );
        assert!(
            !same_incarnation(
                4242,
                WHEN,
                &Observed {
                    pgid: 77,
                    ..alive()
                }
            ),
            "it must still lead the group we recorded"
        );
        assert!(
            !same_incarnation(
                4242,
                "",
                &Observed {
                    started: String::new(),
                    ..alive()
                }
            ),
            "and an unknown start time is not an identity, even when the \
             live process reports one just as empty"
        );
    }

    /// A legacy rendering that does not match proves nothing about
    /// reuse, because nobody wrote down the environment it was
    /// rendered in.
    #[test]
    fn a_legacy_mismatch_is_unmatched_and_never_different() {
        let stranger = Look::Alive(Observed {
            started: "Thu 1 Oct 17:04:00 2026".into(),
            ..alive()
        });
        assert_eq!(
            legacy_leader(4242, WHEN, Look::Alive(alive())),
            Leader::Same
        );
        assert_eq!(legacy_leader(4242, WHEN, stranger), Leader::Unmatched);
        assert_eq!(legacy_leader(4242, WHEN, Look::Gone), Leader::Gone);
        assert_eq!(legacy_leader(4242, WHEN, Look::Unknown), Leader::Unknown);
    }

    /// `ps` output this build cannot read must not read as death.
    #[test]
    fn an_unreadable_look_is_unknown_and_an_absent_pid_is_gone() {
        let me = std::process::id() as i32;
        let Look::Alive(seen) = look(me) else {
            panic!("this process is running")
        };
        assert!(!seen.started.is_empty(), "{seen:?}");
        assert_eq!(
            look(me),
            Look::Alive(seen),
            "and the same look twice gives the same incarnation"
        );
        assert_eq!(look(0), Look::Gone, "pid 0 is not a process to find");

        // A pid that really has gone, in range and once real. Death is
        // settled by `ESRCH` and not by reading `ps`, whose failure for
        // an absent pid is indistinguishable from its failure for a
        // query it did not understand — on this platform both exit
        // non-zero and both write to stderr.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id() as i32;
        child.wait().unwrap();
        assert_eq!(
            look(dead),
            Look::Gone,
            "an exited pid must read as gone, not as a failed look"
        );
        assert_eq!(
            group_of(Pgid::new(dead).expect("a real pid is a real group")),
            Group::Empty,
            "and so must its group"
        );
    }

    #[test]
    fn our_own_group_is_occupied() {
        // SAFETY: a plain query, no memory effects.
        let mine = unsafe { libc::getpgid(0) };
        assert_eq!(
            group_of(Pgid::new(mine).expect("we are in a real group")),
            Group::Occupied
        );
    }

    #[test]
    fn a_group_we_may_not_signal_is_not_an_empty_one() {
        use crate::signal::Probe;
        assert_eq!(group_from(Probe::Ours), Group::Occupied);
        assert_eq!(group_from(Probe::Gone), Group::Empty);
        assert_eq!(
            group_from(Probe::Theirs),
            Group::Occupied,
            "it exists and is somebody else's, which is not empty"
        );
        assert_eq!(
            group_from(Probe::Unknown(libc::EINVAL)),
            Group::Unknown,
            "an unexpected errno is not an answer"
        );
    }

    #[test]
    fn only_a_live_leader_identifies_a_group() {
        assert_eq!(standing(&Leader::Same, &Group::Occupied), Standing::Ours);
        assert_eq!(standing(&Leader::Gone, &Group::Empty), Standing::Gone);
        assert_eq!(
            standing(&Leader::Gone, &Group::Occupied),
            Standing::Unidentified,
            "members with no leader to identify them by are not ours by \
             default: our group may have ended long ago and the number \
             been re-issued to a group whose leader has also exited"
        );
        for group in [Group::Occupied, Group::Empty] {
            assert_eq!(
                standing(&Leader::Different, &group),
                Standing::Theirs,
                "a live leader of another incarnation, by an identity no \
                 environment varies, means the id was reused after ours ended"
            );
            assert_eq!(
                standing(&Leader::Unmatched, &group),
                Standing::Unrecognised,
                "an unmatched legacy rendering is not that proof"
            );
        }
        assert_eq!(
            standing(&Leader::Same, &Group::Empty),
            Standing::Unsure,
            "a live leader in an empty group is not something to act on"
        );
        for group in [Group::Empty, Group::Occupied, Group::Unknown] {
            assert_eq!(standing(&Leader::Unknown, &group), Standing::Unsure);
        }
        assert_eq!(standing(&Leader::Same, &Group::Unknown), Standing::Unsure);
    }

    /// Scripted observations, so a sequence can be driven.
    ///
    /// The leader and the group are scripted SEPARATELY, because the
    /// code asks them separately. A script that runs out repeats its
    /// last answer, so a test lists only the observations it cares
    /// about. Every group observation also notes whether the record
    /// still existed at that moment.
    struct Scripted {
        leaders: std::cell::RefCell<(std::vec::IntoIter<Leader>, Leader)>,
        groups: std::cell::RefCell<(std::vec::IntoIter<Group>, Group)>,
        sent: std::cell::RefCell<Vec<(i32, libc::c_int)>>,
        record_at_each_group_look: std::cell::RefCell<Vec<(Group, bool)>>,
        record: std::path::PathBuf,
        looked: std::cell::Cell<bool>,
        reads: std::cell::Cell<u32>,
        started: std::time::Instant,
        took: Duration,
    }

    impl Scripted {
        fn new(dir: &std::path::Path, leaders: Vec<Leader>, groups: Vec<Group>) -> Self {
            Self {
                leaders: std::cell::RefCell::new((leaders.into_iter(), Leader::Unknown)),
                groups: std::cell::RefCell::new((groups.into_iter(), Group::Unknown)),
                sent: Default::default(),
                record_at_each_group_look: Default::default(),
                record: producer_path(dir, "src-1"),
                looked: Default::default(),
                reads: Default::default(),
                started: std::time::Instant::now(),
                took: Duration::ZERO,
            }
        }
    }

    impl Os for Scripted {
        fn leader(&self, _: &Producer) -> Leader {
            self.looked.set(true);
            let mut s = self.leaders.borrow_mut();
            if let Some(next) = s.0.next() {
                s.1 = next;
            }
            s.1.clone()
        }
        fn group(&self, _: crate::signal::Pgid) -> Group {
            let mut s = self.groups.borrow_mut();
            if let Some(next) = s.0.next() {
                s.1 = next;
            }
            self.record_at_each_group_look
                .borrow_mut()
                .push((s.1.clone(), self.record.exists()));
            s.1.clone()
        }
        // Recorded as plain numbers, so an assertion about what was
        // signalled names the GROUP without the type in the way.
        fn signal(&self, pgid: crate::signal::Pgid, sig: libc::c_int) {
            self.sent.borrow_mut().push((pgid.get(), sig));
        }
        fn collect(&self) {}
        fn pause(&self, _: Duration) {}
        // Injected, because the hazard is precisely that `sleep`
        // reports a minimum and the world supplies the maximum. The
        // first read is the identification; every one after it is the
        // gap being measured.
        fn now(&self) -> std::time::Instant {
            self.reads.set(self.reads.get() + 1);
            if self.reads.get() <= 1 {
                self.started
            } else {
                self.started + self.took
            }
        }
    }

    fn reap_over(
        dir: &std::path::Path,
        leaders: Vec<Leader>,
        groups: Vec<Group>,
    ) -> (Retired, Vec<(i32, libc::c_int)>) {
        reap_over_taking(dir, leaders, groups, Duration::ZERO)
    }

    fn reap_over_taking(
        dir: &std::path::Path,
        leaders: Vec<Leader>,
        groups: Vec<Group>,
        took: Duration,
    ) -> (Retired, Vec<(i32, libc::c_int)>) {
        let os = Scripted {
            took,
            ..Scripted::new(dir, leaders, groups)
        };
        let out = reap_previous_producer_with(dir, "src-1", &os);
        (out, os.sent.into_inner())
    }

    #[test]
    fn a_recorded_orphan_is_terminated_then_killed_and_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        recorded(dir.path());

        let (out, sent) = reap_over(
            dir.path(),
            vec![Leader::Same, Leader::Gone],
            vec![Group::Occupied, Group::Occupied, Group::Empty],
        );
        assert_eq!(
            sent,
            vec![(4242, libc::SIGTERM), (4242, libc::SIGKILL)],
            "gently first: it is the user's own command and may have \
             state worth flushing"
        );
        assert_eq!(out, Retired::Clear, "and the launch is authorised");
        assert!(
            !producer_path(dir.path(), "src-1").exists(),
            "the record goes, so a successor does not signal it again"
        );
    }

    /// The shell exits on the `SIGTERM` and a descendant it
    /// backgrounded ignores it.
    ///
    /// The escalation asks about the GROUP rather than the leader —
    /// sound because the group was IDENTIFIED moments earlier in this
    /// same attempt, and a pid is not re-issued inside a fraction of a
    /// second. A cold record carries no such bound, which is why one
    /// whose leader is already gone is refused instead.
    #[test]
    fn a_descendant_that_outlives_its_leader_is_still_ended() {
        let dir = tempfile::tempdir().unwrap();
        recorded(dir.path());

        let (out, sent) = reap_over(
            dir.path(),
            // Identified, then the leader goes while the group does not.
            vec![Leader::Same, Leader::Gone],
            vec![Group::Occupied, Group::Occupied, Group::Empty],
        );
        assert_eq!(
            sent,
            vec![(4242, libc::SIGTERM), (4242, libc::SIGKILL)],
            "a leader that exited on the term must not spare the group"
        );
        assert_eq!(out, Retired::Clear, "and the source may run again");
    }

    /// A group that outlasts the kill defers rather than replacing.
    #[test]
    fn a_group_that_will_not_end_withholds_the_launch() {
        let dir = tempfile::tempdir().unwrap();
        recorded(dir.path());

        let (out, sent) = reap_over(dir.path(), vec![Leader::Same], vec![Group::Occupied]);
        assert_eq!(sent, vec![(4242, libc::SIGTERM), (4242, libc::SIGKILL)]);
        assert!(matches!(out, Retired::Deferred(_)), "{out:?}");
        assert!(
            producer_path(dir.path(), "src-1").exists(),
            "and the record stays: the group it names is still running"
        );
    }

    /// A replacement leader appearing between the term and the
    /// escalation cancels it.
    ///
    /// The escalation used to ask only whether the group was occupied,
    /// so a live leader with an obviously different start time was not
    /// read until AFTER the kill had been sent — to that leader's
    /// group (codex on 1d90b9b).
    #[test]
    fn a_replacement_leader_between_the_term_and_the_kill_cancels_it() {
        let dir = tempfile::tempdir().unwrap();
        recorded(dir.path());

        let (out, sent) = reap_over(
            dir.path(),
            vec![
                Leader::Same,
                // Our group ended and the number is already somebody
                // else's, with a leader that says so.
                Leader::Different,
            ],
            vec![Group::Occupied],
        );
        assert_eq!(
            sent,
            vec![(4242, libc::SIGTERM)],
            "only the term: the id belongs to another group now"
        );
        assert_eq!(out, Retired::Clear, "and ours must therefore have ended");
    }

    /// An attempt that took arbitrarily long does not escalate on
    /// occupancy alone.
    ///
    /// `sleep(200ms)` is a MINIMUM. A process stopped between the term
    /// and the escalation resumes with its sleep over and any amount of
    /// the world's time behind it, by which point the id may name a
    /// group it never owned. Only a measured gap notices, and when the
    /// identification has aged out this fails closed.
    #[test]
    fn an_attempt_that_took_too_long_does_not_escalate() {
        let dir = tempfile::tempdir().unwrap();
        recorded(dir.path());

        let (out, sent) = reap_over_taking(
            dir.path(),
            // Identified, then the leader is gone and members remain —
            // which is the one case that rests on the gap being short.
            vec![Leader::Same, Leader::Gone],
            vec![Group::Occupied],
            IDENTIFICATION_KEEPS + Duration::from_millis(1),
        );
        assert_eq!(
            sent,
            vec![(4242, libc::SIGTERM)],
            "the identification aged out, so no kill may follow it"
        );
        assert!(matches!(out, Retired::Deferred(_)), "{out:?}");
        assert!(
            producer_path(dir.path(), "src-1").exists(),
            "and nothing is replaced on evidence that expired"
        );

        // The same observations, promptly, DO escalate — so it is the
        // elapsed time deciding and not the shape of the sequence.
        let (out, sent) = reap_over_taking(
            dir.path(),
            vec![Leader::Same, Leader::Gone],
            vec![Group::Occupied, Group::Occupied, Group::Empty],
            IDENTIFICATION_KEEPS,
        );
        assert_eq!(sent, vec![(4242, libc::SIGTERM), (4242, libc::SIGKILL)]);
        assert_eq!(out, Retired::Clear);
    }

    /// The escalation decision, on its own.
    #[test]
    fn only_a_current_identity_or_a_fresh_one_may_escalate() {
        let keeps = Duration::from_secs(1);
        let prompt = Duration::from_millis(10);
        let late = keeps + Duration::from_millis(1);

        // A leader that is still ours needs no freshness argument.
        assert!(may_escalate(&Standing::Ours, late, keeps));
        // A leaderless group rests entirely on one.
        assert!(may_escalate(&Standing::Unidentified, prompt, keeps));
        assert!(may_escalate(&Standing::Unidentified, keeps, keeps));
        assert!(!may_escalate(&Standing::Unidentified, late, keeps));
        // And the rest are not evidence at all.
        for s in [
            Standing::Theirs,
            Standing::Gone,
            Standing::Unrecognised,
            Standing::Unsure,
        ] {
            assert!(!may_escalate(&s, prompt, keeps), "{s:?}");
        }
    }

    /// A STALE record over a re-issued group whose leader has ALSO
    /// ended: members, no leader, and none of it ours.
    ///
    /// The sequence the "an id is not reused while its group has
    /// members" guarantee does not cover — it bounds reuse during a
    /// lifetime, not the gap since a record was written. Our group
    /// ended, the number went to a group we know nothing about, and
    /// that group's leader exited too. What remains looks exactly like
    /// our own surviving descendants, and killing it would be killing
    /// a stranger's processes (codex on aef11a3).
    #[test]
    fn a_recycled_group_whose_leader_also_ended_is_never_signalled() {
        let dir = tempfile::tempdir().unwrap();
        recorded(dir.path());

        let (out, sent) = reap_over(dir.path(), vec![Leader::Gone], vec![Group::Occupied]);
        assert!(
            sent.is_empty(),
            "nothing identifies these members as ours, so nothing may be sent"
        );
        assert!(
            matches!(out, Retired::Deferred(_)),
            "and being unable to identify them is not permission to \
             replace them either: {out:?}"
        );
        assert!(
            producer_path(dir.path(), "src-1").exists(),
            "the record stays, and says what to do about it"
        );
        let Retired::Deferred(why) = out else {
            unreachable!()
        };
        assert!(why.contains("no leader to identify them by"), "{why}");
    }

    #[test]
    fn nothing_is_signalled_for_a_group_that_is_not_ours() {
        let dir = tempfile::tempdir().unwrap();

        // No record at all: the ordinary first launch.
        let (out, sent) = reap_over(dir.path(), vec![], vec![]);
        assert!(sent.is_empty());
        assert_eq!(out, Retired::Clear);

        // A record whose group has provably gone.
        recorded(dir.path());
        let (out, sent) = reap_over(dir.path(), vec![Leader::Gone], vec![Group::Empty]);
        assert!(sent.is_empty(), "a group that has ended needs no signal");
        assert_eq!(out, Retired::Clear);
        assert!(!producer_path(dir.path(), "src-1").exists());

        // A live leader of another incarnation: the id was reused, so
        // ours ended, and whatever is in that group now is theirs.
        recorded(dir.path());
        let (out, sent) = reap_over(dir.path(), vec![Leader::Different], vec![Group::Occupied]);
        assert!(sent.is_empty(), "a recycled id must never be signalled");
        assert_eq!(out, Retired::Clear);

        // A look that FAILED keeps the record AND withholds the
        // launch: the record is the only evidence the group exists,
        // and a launch would overwrite it on its way past.
        recorded(dir.path());
        let (out, sent) = reap_over(dir.path(), vec![Leader::Unknown], vec![Group::Empty]);
        assert!(sent.is_empty());
        assert!(matches!(out, Retired::Deferred(_)), "{out:?}");
        assert!(producer_path(dir.path(), "src-1").exists());

        // A group whose state cannot be established, likewise.
        let (out, _) = reap_over(dir.path(), vec![Leader::Gone], vec![Group::Unknown]);
        assert!(matches!(out, Retired::Deferred(_)), "{out:?}");
        assert!(producer_path(dir.path(), "src-1").exists());
    }

    /// A record naming a special id is refused before anything is
    /// looked at, let alone signalled.
    ///
    /// `kill(-1, …)` is every process this user may signal rather than
    /// group 1, and `record_producer` cannot write such a record — a
    /// child's pid is never 0 or 1 — so reaching this means evidence
    /// that did not come from here, and the only safe response is to
    /// touch nothing.
    #[test]
    fn a_record_naming_a_special_id_signals_nothing() {
        for pgid in [1i32, 0, -1] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(producer_path(dir.path(), "src-1"), legacy_bytes(pgid, WHEN)).unwrap();

            let os = Scripted::new(dir.path(), vec![Leader::Same], vec![Group::Occupied]);
            let out = reap_previous_producer_with(dir.path(), "src-1", &os);
            assert!(os.sent.borrow().is_empty(), "pgid {pgid} was signalled");
            assert!(
                !os.looked.get(),
                "pgid {pgid} was not even worth looking at"
            );
            assert!(matches!(out, Retired::Deferred(_)), "pgid {pgid}: {out:?}");
        }
    }

    /// Unreadable evidence is unknown prior state, not absence.
    #[test]
    fn evidence_that_cannot_be_read_withholds_the_launch() {
        let dir = tempfile::tempdir().unwrap();

        // Malformed, which the rename should make unreachable — but if
        // it is ever reached, overwriting it is the one thing that
        // must not happen.
        std::fs::write(producer_path(dir.path(), "src-1"), b"not json").unwrap();
        let (out, sent) = reap_over(dir.path(), vec![], vec![]);
        assert!(sent.is_empty());
        assert!(matches!(out, Retired::Deferred(_)), "{out:?}");
        assert!(
            producer_path(dir.path(), "src-1").exists(),
            "an unusable record is kept, and the message names it"
        );
        let Retired::Deferred(why) = out else {
            unreachable!()
        };
        assert!(why.contains("src-1.producer"), "{why}");

        // A read that fails for any other reason — here, a DIRECTORY
        // where the record belongs — is not absence either.
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(producer_path(other.path(), "src-1")).unwrap();
        let (out, sent) = reap_over(other.path(), vec![], vec![]);
        assert!(sent.is_empty());
        assert!(
            matches!(out, Retired::Deferred(_)),
            "a read that failed is not a first launch: {out:?}"
        );
    }

    /// Published through a rename, since this is the evidence that has
    /// to survive the SIGKILL it exists for: a truncating write caught
    /// mid-flight would leave a record that parses as nothing.
    ///
    /// The atomicity itself is NOT what this asserts — a write
    /// interrupted mid-flight cannot be provoked from a test here, and
    /// a mutation to a plain `fs::write` survives. What is asserted is
    /// what can be: the record round-trips, no temp file is left
    /// behind, and a publication that fails says so instead of
    /// pretending.
    #[test]
    fn a_record_is_published_whole_or_not_at_all() {
        let dir = tempfile::tempdir().unwrap();
        recorded(dir.path());
        let back: Producer =
            serde_json::from_slice(&std::fs::read(producer_path(dir.path(), "src-1")).unwrap())
                .unwrap();
        assert_eq!(
            back,
            Producer::Anchored {
                pgid: 4242,
                birth: birth()
            }
        );
        assert!(
            !producer_path(dir.path(), "src-1")
                .with_extension("producer.tmp")
                .exists(),
            "and the temp file does not linger"
        );

        // An unwritable directory is reported, not swallowed: the
        // caller decides whether to run an unrecorded producer.
        let gone = dir.path().join("not-there");
        assert!(
            record_producer(
                &gone,
                "src-1",
                &Producer::Anchored {
                    pgid: 4242,
                    birth: birth()
                }
            )
            .is_err()
        );
    }

    /// The record outlives the group's last member, never the other
    /// way round: a host that dies anywhere inside a retirement leaves
    /// its successor the record of whatever it had not yet ended.
    #[test]
    fn the_record_outlives_every_member_of_its_group() {
        let dir = tempfile::tempdir().unwrap();
        recorded(dir.path());
        let os = Scripted::new(
            dir.path(),
            vec![Leader::Same, Leader::Same, Leader::Same, Leader::Gone],
            vec![
                Group::Occupied,
                Group::Occupied,
                Group::Occupied,
                Group::Empty,
            ],
        );
        let out = reap_previous_producer_with(dir.path(), "src-1", &os);
        assert_eq!(out, Retired::Clear);
        let looks = os.record_at_each_group_look.into_inner();
        assert!(looks.iter().any(|(g, _)| *g == Group::Occupied));
        for (group, record_existed) in &looks {
            if *group == Group::Occupied {
                assert!(
                    record_existed,
                    "the record went while members remained: {looks:?}"
                );
            }
        }
        assert!(!producer_path(dir.path(), "src-1").exists());
    }

    #[test]
    fn a_legacy_record_reads_as_the_bytes_an_earlier_clank_wrote() {
        let back: Producer = serde_json::from_slice(&legacy_bytes(4242, WHEN)).unwrap();
        assert_eq!(
            back,
            Producer::Legacy {
                pgid: 4242,
                started: WHEN.into()
            }
        );
    }

    #[test]
    fn a_record_of_an_unknown_version_withholds_the_launch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            producer_path(dir.path(), "src-1"),
            br#"{"version":9,"pgid":4242,"birth":{"v":"mac1","sec":1,"usec":2}}"#,
        )
        .unwrap();
        let (out, sent) = reap_over(dir.path(), vec![Leader::Same], vec![Group::Occupied]);
        assert!(sent.is_empty());
        let Retired::Deferred(why) = out else {
            panic!("an unreadable version is not a first launch: {out:?}")
        };
        assert!(why.contains("version 9"), "{why}");
        assert!(producer_path(dir.path(), "src-1").exists());
    }

    /// A live process, `ps`-rendered under `tz`.
    fn lstart_under(pid: i32, tz: &str) -> String {
        let out = std::process::Command::new("ps")
            .args(["-o", "lstart=", "-p"])
            .arg(pid.to_string())
            .env("TZ", tz)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// A live producer recorded by an earlier clank under another
    /// `TZ`: the same process, read differently. That is not reuse, so
    /// it is neither signalled nor replaced.
    #[test]
    fn a_legacy_record_rendered_under_another_tz_defers() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("300")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let Look::Alive(here) = look(pid) else {
            panic!("the child is running")
        };
        let elsewhere = ["Pacific/Kiritimati", "Etc/GMT+12"]
            .into_iter()
            .map(|tz| lstart_under(pid, tz))
            .find(|rendered| !rendered.is_empty() && *rendered != here.started)
            .expect("one of two zones 26 hours apart renders differently");
        std::fs::write(
            producer_path(dir.path(), "src-1"),
            legacy_bytes(pid, &elsewhere),
        )
        .unwrap();

        let out = reap_previous_producer(dir.path(), "src-1");
        let Retired::Deferred(why) = out else {
            panic!("an unmatched legacy record was treated as settled: {out:?}")
        };
        assert!(why.contains("src-1.producer"), "{why}");
        assert!(
            child.try_wait().unwrap().is_none(),
            "our own producer was signalled for rendering differently"
        );
        assert!(producer_path(dir.path(), "src-1").exists());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// The legacy collision a guessed environment would authorise: a
    /// Sydney record of an 08:45 UTC start reads `19:45`, and the
    /// stranger that later took the pid at 19:45 UTC reads exactly that
    /// in UTC. In the caller's own (Sydney) environment it reads
    /// otherwise. Pure, so no real process is endangered.
    #[test]
    fn a_reused_pid_matching_under_a_guessed_tz_is_never_signalled() {
        struct SydneyCaller {
            sent: std::cell::RefCell<Vec<libc::c_int>>,
        }
        impl Os for SydneyCaller {
            fn leader(&self, record: &Producer) -> Leader {
                let Producer::Legacy { pgid, started } = record else {
                    unreachable!()
                };
                legacy_leader(
                    *pgid,
                    started,
                    Look::Alive(Observed {
                        pgid: *pgid,
                        started: "Tue 6 Oct 06:45:00 2026".into(),
                    }),
                )
            }
            fn group(&self, _: crate::signal::Pgid) -> Group {
                Group::Occupied
            }
            fn signal(&self, _: crate::signal::Pgid, sig: libc::c_int) {
                self.sent.borrow_mut().push(sig);
            }
            fn collect(&self) {}
            fn pause(&self, _: Duration) {}
            fn now(&self) -> std::time::Instant {
                std::time::Instant::now()
            }
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            producer_path(dir.path(), "src-1"),
            legacy_bytes(4242, "Mon 5 Oct 19:45:00 2026"),
        )
        .unwrap();
        let os = SydneyCaller {
            sent: Default::default(),
        };
        let out = reap_previous_producer_with(dir.path(), "src-1", &os);
        assert!(os.sent.borrow().is_empty(), "a stranger was signalled");
        assert!(matches!(out, Retired::Deferred(_)), "{out:?}");
        assert!(producer_path(dir.path(), "src-1").exists());
    }

    /// An anchored record's identity is read as numbers, so there is
    /// no environment for a reader to differ in — and it still tells a
    /// reused pid apart.
    #[test]
    fn an_anchored_identity_has_no_rendering_and_still_tells_reuse_apart() {
        use crate::proc_identity::ProcToken;
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("300")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let birth = crate::proc_identity::token_for(pid).unwrap();
        let ours = Producer::Anchored {
            pgid: pid,
            birth: birth.clone(),
        };
        record_producer(dir.path(), "src-1", &ours).unwrap();
        let bytes = std::fs::read_to_string(producer_path(dir.path(), "src-1")).unwrap();
        assert!(!bytes.contains("started"), "{bytes}");
        assert_eq!(leader_of(&ours), Leader::Same);

        let reused = Producer::Anchored {
            pgid: pid,
            birth: match birth {
                ProcToken::MacStart { sec, usec } => ProcToken::MacStart {
                    sec,
                    usec: usec + 1,
                },
                ProcToken::LinuxStart { ticks, boot } => ProcToken::LinuxStart {
                    ticks: ticks + 1,
                    boot,
                },
                ProcToken::Unrecognised => unreachable!(),
            },
        };
        assert_eq!(leader_of(&reused), Leader::Different);
        record_producer(dir.path(), "src-1", &reused).unwrap();
        assert_eq!(reap_previous_producer(dir.path(), "src-1"), Retired::Clear);
        assert!(
            child.try_wait().unwrap().is_none(),
            "a different incarnation's process was signalled"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
