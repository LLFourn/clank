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

/// Every producer id this source has already turned into an event.
///
/// ONLY an index. The event itself — payload, acked state, the whole
/// lifecycle — lives in the same `EventLog` github uses, so
/// `clank events list/show/ack` finds a command event by the same
/// directory glob and an unacked one is never compacted away. Writing
/// a parallel store was how an earlier version produced events that
/// could be woken on and never handled (codex a43bd85).
///
/// TWO TIERS, because the log and the index can disagree and the log
/// is the authority:
///
/// - `warm` holds what the WAL still vouches for, recovered from its
///   retained identities at arm time, plus everything ingested since.
/// - the directory holds one file per id, unbounded, which is what
///   survives past the WAL's `LOG_IDENTITY_CAP`.
///
/// Neither alone is enough. The file tier is best-effort — a failed
/// write must not let a replay append a second row — and the warm
/// tier cannot outlive the arm. A crash between the log append and
/// the index write leaves an id the WAL knows and the directory does
/// not, and [`Seen::recover`] is what repairs that WITHOUT minting a
/// second event (codex 43f5ed8).
pub(crate) struct Seen {
    dir: std::path::PathBuf,
    warm: std::collections::HashSet<String>,
}

/// What an id's index entry says.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Known {
    /// Never ingested.
    No,
    /// Ingested; its event is in the log.
    Yes,
    /// The entry exists but cannot be read. Treated as NOT seen, so a
    /// replay repairs it: the alternative is what an earlier version
    /// did — suppress the producer's valid replay on behalf of a
    /// record nobody can read, losing the event permanently and
    /// silently (codex a43bd85).
    Damaged(String),
}

impl Seen {
    pub(crate) fn at(events_dir: &std::path::Path, source_key: &str) -> Self {
        Self {
            dir: events_dir.join(format!("{source_key}.seen")),
            warm: std::collections::HashSet::new(),
        }
    }

    /// Reconcile against the WAL's own retained identities.
    ///
    /// The log is authoritative: an id it still accounts for HAS an
    /// event, whatever the derived index says. Called once per arm,
    /// before anything is ingested, so the crash gap between an
    /// append and its index write is closed by the next arm rather
    /// than duplicating the event.
    ///
    /// Repairs the durable tier too, best-effort — a failure there
    /// costs nothing this arm, because `warm` already carries it.
    pub(crate) fn recover(&mut self, ids: impl IntoIterator<Item = String>) {
        for id in ids {
            if self.known(&id) != Known::Yes {
                let _ = self.write(&id);
            }
            self.warm.insert(id);
        }
    }

    /// Hashed, because an id is arbitrary producer text and a
    /// filename is not: it may be long, contain separators, or differ
    /// from another id only in case on a case-folding filesystem.
    fn file(&self, id: &str) -> std::path::PathBuf {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in id.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.dir.join(format!("{h:016x}"))
    }

    /// The entry holds the id VERBATIM, so a read can tell "this id"
    /// from a hash collision, and a damaged entry from a missing one.
    pub(crate) fn known(&self, id: &str) -> Known {
        if self.warm.contains(id) {
            return Known::Yes;
        }
        let path = self.file(id);
        match std::fs::read_to_string(&path) {
            Ok(text) if text == id => Known::Yes,
            Ok(other) => Known::Damaged(format!(
                "{} holds `{}`, not `{id}`",
                path.display(),
                other.chars().take(40).collect::<String>()
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Known::No,
            Err(e) => Known::Damaged(format!("{}: {e}", path.display())),
        }
    }

    /// Remember `id`. The warm tier is updated even when the durable
    /// write fails, so a disk problem costs a duplicate at the NEXT
    /// arm at worst — never one in this one.
    pub(crate) fn record(&mut self, id: &str) -> std::io::Result<()> {
        self.warm.insert(id.to_string());
        self.write(id)
    }

    /// Discard everything — the log this index describes is gone.
    ///
    /// Only for quarantine. The index is derived, so a log moved
    /// aside leaves it describing events that no longer exist, and
    /// an entry that outlives its event refuses the producer's
    /// replay of it (codex 072e459). Forgetting is the safe
    /// direction: at worst a backend that replays old ids produces
    /// duplicates, which is the trade this whole plan already makes.
    pub(crate) fn forget_all(&mut self) -> std::io::Result<()> {
        self.warm.clear();
        match std::fs::remove_dir_all(&self.dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    fn write(&self, id: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.file(id);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, id.as_bytes())?;
        std::fs::rename(&tmp, &path)
    }
}

/// The private directory a backend keeps its own state in —
/// `.clank/agents/<label>/sources/<name>/`, handed over in
/// `CLANK_SOURCE_STATE_DIR`.
///
/// Clank creates it and NEVER reads it. Whatever cursor, cache or
/// baseline a backend needs lives here, which is the counterpart of
/// clank no longer holding a resume point on its behalf.
pub(crate) fn backend_state_dir(
    agent_dir: &std::path::Path,
    src: &clank_core::agent_config::CommandSource,
) -> std::path::PathBuf {
    // The SOURCE KEY, not a slug of the name. A slug is lossy the
    // same way it was for the key — `ci/deploy` and `ci-deploy`
    // collide, and two backends would share one private cache — and
    // worse here, because a name of `..` slugs to `..` and resolves
    // to the agent directory itself (codex 1948041). The key is
    // `command-<slug>-<hash>`, which is a single safe component by
    // construction.
    agent_dir.join("sources").join(src.source_key())
}

/// One line of a producer's stdout, as the wire carries it.
///
/// Presentation is NOT here: the glyph is config, so a producer
/// cannot reach the renderer's layout.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct Line {
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

/// Whether a configured glyph can be drawn in the one cell the row
/// reserves for it.
///
/// A two-cell glyph misaligns every row after it, a control character
/// moves the cursor, and a base-plus-selector pair can be split by
/// truncation.
///
/// **EVERY reader of a configured glyph calls this, including the
/// ones that only display.** This doc used to say the check happened
/// at config load and that "the only way to get a bad glyph on screen
/// is to not start at all" — which was the false invariant that
/// produced a hole: the log and the TUI read the CURRENT config,
/// independently of any wait, so an operator editing a glyph after
/// the fact reached the renderer with a newline and nothing had
/// checked it (codex 9a53f39). Validating at load does not make a
/// value validated later; provenance is not a property of a value.
/// A new reader of `CommandSource::glyph` calls this too.
pub(crate) fn glyph_refusal(glyph: &str) -> Option<String> {
    use unicode_width::UnicodeWidthChar;

    // Unicode data, not a hand-listed set. The first attempt at this
    // was a denylist, and it missed U+05B0, U+061C, U+FE20 and
    // U+E0100 (codex 408c756) — the same way this repo's own WIDE
    // table records being wrong twice before it was generated from
    // the UCD. A property boundary written by hand is a property
    // boundary with holes.
    let mut chars = glyph.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return Some(if glyph.is_empty() {
            "is empty".to_string()
        } else {
            format!(
                "is {} characters; a glyph is exactly one",
                glyph.chars().count()
            )
        });
    };
    // `width()` is None for exactly the C0/C1 controls: a newline or
    // an ESC moves the cursor instead of filling the cell.
    let Some(w) = c.width() else {
        return Some(format!(
            "U+{:04X} is a control character; it moves the cursor rather than filling a cell",
            c as u32
        ));
    };
    // Redundant with the width test below for the VERDICT — 0 is not
    // 1 either way — and kept for the message, which is the part a
    // reader can act on.
    if w == 0 {
        return Some(format!(
            "U+{:04X} has no visible form of its own; it modifies the character before it",
            c as u32
        ));
    }
    if w != 1 {
        return Some(format!(
            "U+{:04X} measures {w} display columns; the row reserves one",
            c as u32
        ));
    }
    // And the RENDERER has to agree, because it is what lays the row
    // out. UNREACHABLE today and measured so: no scalar in the whole
    // range has `unicode-width` 1 and renderer 2, which is why this
    // cannot currently refuse anything the check above accepted. It
    // stays because the two tables are versioned independently — the
    // renderer's is generated from UCD 16.0.0, the crate tracks its
    // own — and a drift would let a glyph validated as one cell be
    // drawn as two. `the_two_width_models_agree_wherever_it_matters`
    // is what notices if that day comes.
    let cells = crate::cli::status_tui::text::display_width(glyph);
    if cells != 1 {
        return Some(format!(
            "measures {cells} display columns to the renderer; the row reserves one"
        ));
    }
    None
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
        id: line.id.clone(),
        item: Box::new(clank_core::wait::WaitItem::CommandEvent {
            name: src.name.clone(),
            id: line.id,
            // Defaults to the source's name: with nothing filtering on
            // it, a producer that omits it means "one of mine".
            event: kind.unwrap_or_else(|| src.name.clone()),
            // Untrusted text on its way to a TUI row and a web page:
            // first line only, control bytes dropped.
            content: crate::cli::status_tui::text::one_line(
                line.content.as_deref().unwrap_or(""),
                usize::MAX,
            ),
            at: line.at,
            failure: false,
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
        Ingested::Emit { id, mut item } => {
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
            let seq = match log.append_inbox(
                crate::cli::github::log::Transport::Command,
                Some(&id),
                None,
                event_at,
                &item,
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
            stamp_selector(&mut item, source_key, seq);
            send(*item)
        }
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
    if let clank_core::wait::WaitItem::CommandEvent { selector, .. } = item {
        *selector = Some(crate::cli::events::Selector::qualified(source_key, seq).to_string());
    }
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
        name: src.name.clone(),
        id: id.to_string(),
        event: FAILURE_KIND.to_string(),
        content: crate::cli::status_tui::text::one_line(why, usize::MAX),
        at: None,
        failure: true,
        selector: None,
    }
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
            name: name.into(),
            command: vec!["true".into()],
            glyph: None,
        }
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
        let entry = seen.file("1");
        std::fs::write(&entry, b"some other id").unwrap();
        let Known::Damaged(why) = fresh().known("1") else {
            panic!("an entry that does not hold this id is damage, not a hit")
        };
        assert!(why.contains(&entry.display().to_string()), "{why}");
        assert!(why.contains('1'), "the id must be named too: {why}");

        // A read ERROR is damage as well — not absence. A directory
        // where the entry belongs is the cheapest way to make
        // `read_to_string` fail with something that is not NotFound.
        std::fs::create_dir_all(fresh().file("2")).unwrap();
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
        let src = src_named("s");
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

    /// Content is untrusted text: one line, no control bytes.
    #[test]
    fn content_cannot_carry_control_bytes_into_a_row() {
        let src = src_named("s");
        let dir = tmpdir();
        let seen = Seen::at(dir.path(), KEY);
        let what = ingest_line(
            &src,
            &seen,
            r#"{"id":"1","kind":"k","content":"first\u001b[31mred\nsecond"}"#,
        )
        .expect("not blank");
        let Ingested::Emit { item, .. } = what else {
            panic!("expected an emit")
        };
        let WaitItem::CommandEvent { content, .. } = *item else {
            panic!("expected a command event")
        };
        assert!(!content.contains('\u{1b}'), "no escapes: {content:?}");
        assert!(!content.contains('\n'), "one line only: {content:?}");
        assert!(content.starts_with("first") && !content.contains("second"));
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
        let at = |n: &str| backend_state_dir(agent, &src_named(n));

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

    /// Width alone would accept things that are not glyphs.
    ///
    /// `display_width` counts every non-wide scalar as one, by
    /// design — so this check cannot be a width check (codex
    /// c6dc55f).
    ///
    /// The boundary is `unicode-width`'s judgement, not a list of
    /// mine. U+110BD KAITHI NUMBER SIGN is the case that taught me
    /// the difference: I assumed a format character must be invisible
    /// and asserted it here, and the library reports width 1. Taking
    /// the dependency and then hand-patching its answers would be the
    /// denylist again with extra steps.
    #[test]
    fn a_glyph_that_measures_one_cell_can_still_be_no_glyph() {
        for (bad, what) in [
            ("\n", "newline"),
            ("\t", "tab"),
            ("\u{1b}", "ESC"),
            ("\u{0301}", "a lone combining acute"),
            ("\u{200D}", "a zero-width joiner"),
            ("\u{FE0F}", "a variation selector"),
            ("\u{05B0}", "HEBREW POINT SHEVA"),
            ("\u{061C}", "ARABIC LETTER MARK"),
            ("\u{FE20}", "COMBINING LIGATURE LEFT HALF"),
            ("\u{E0100}", "VARIATION SELECTOR-17"),
            ("\u{0F71}", "TIBETAN VOWEL SIGN AA"),
            ("\u{1AB0}", "COMBINING DOUBLED CIRCUMFLEX"),
            ("\u{2064}", "INVISIBLE PLUS"),
        ] {
            assert_eq!(
                crate::cli::status_tui::text::display_width(bad),
                1,
                "{what} measures one cell to the RENDERER, which is why its \
                 width cannot be the test",
            );
            assert!(
                glyph_refusal(bad).is_some(),
                "{what} is not a glyph and must be refused",
            );
        }
    }

    #[test]
    fn a_glyph_must_fit_the_cell_the_row_reserves() {
        assert_eq!(glyph_refusal("*"), None);
        assert_eq!(glyph_refusal("→"), None);
        assert!(glyph_refusal("\u{1F4A4}").is_some(), "two cells");
        assert!(
            glyph_refusal("\u{2601}\u{FE0F}").is_some(),
            "base plus selector"
        );
        assert!(glyph_refusal("ab").is_some());
        assert!(glyph_refusal("").is_some());
    }

    /// The cross-check against the renderer is unreachable, and this
    /// is the property that makes it so.
    ///
    /// Measured across the whole scalar range: nothing has
    /// `unicode-width` 1 and renderer width 2. If a table update ever
    /// breaks this, a glyph could validate at one cell and be DRAWN
    /// at two — which is what the otherwise-dead guard is for.
    #[test]
    fn the_two_width_models_agree_wherever_it_matters() {
        use unicode_width::UnicodeWidthChar;
        let mut disagree = Vec::new();
        for cp in 0u32..=0x10FFFF {
            let Some(c) = char::from_u32(cp) else {
                continue;
            };
            let Some(u) = c.width() else { continue };
            if u != 1 {
                continue;
            }
            if crate::cli::status_tui::text::display_width(&c.to_string()) != 1 {
                disagree.push(cp);
            }
        }
        assert!(
            disagree.is_empty(),
            "scalars the crate calls one cell and the renderer does not: {:04X?}",
            &disagree[..disagree.len().min(8)],
        );
    }
}
