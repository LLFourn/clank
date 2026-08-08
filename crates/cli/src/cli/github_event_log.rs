//! The per-agent github event WAL (github-offline-catchup): an
//! append-only JSONL inbox with an explicit handled/unhandled
//! lifecycle. Every ingested github event — poll feed or realtime
//! relay — is appended here BEFORE it is presented, so a crash between
//! append and emit re-presents the event on the next arm
//! (at-least-once, never silent loss).
//!
//! Invariants the format encodes:
//! - **One atomic primary record per observed feed event** (codex
//!   df3f1a7): a PRESENTABLE event appends exactly one `inbox` record
//!   (carrying its feed id); a non-presentable observation (filtered
//!   kind, own action, unclassifiable, action-key-deduped, baseline)
//!   appends exactly one `obs` record. The durable cursor rebuilds
//!   from the identities of BOTH forms, so a single append is always
//!   the complete durable decision for its event — no prefix of the
//!   log can mark an event seen without also holding its presentable
//!   payload.
//! - **Independent identity horizons** (codex 82a221a): poll events
//!   carry a `feed_id`; relay events carry only a content-derived
//!   `action_key` (their payloads have no feed id). Either may be
//!   absent; records persist whichever identities exist so both the
//!   feed-id cursor and the coordinator's action-key horizon rebuild
//!   from the log alone.
//! - **All writers share one sidecar-lock protocol** (codex 899f915):
//!   every append (ingest and ack alike) holds the sidecar lock
//!   SHARED and opens the log AFTER acquiring it; compaction holds it
//!   EXCLUSIVE and replaces the file by rename. Locking a sidecar —
//!   never the log inode itself — means an appender can't write
//!   through a stale fd into a file the compactor just replaced.
//! - **Failure domains are separate** (codex 899f915): `state.json`
//!   is cache metadata (ETag / poll-interval floor); corruption there
//!   resets only the cache. Only the WAL itself corrupting degrades
//!   to a fresh baseline — and a torn FINAL line (no trailing
//!   newline) is a normal crash artifact, dropped alone, while an
//!   unparseable interior line quarantines the whole file to
//!   `<name>.corrupt`.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use clank_core::wait::WaitItem;

/// Newest identities kept for dedup horizons. The github `/events`
/// feed retains only ~300 events, so identities beyond this are dead
/// weight for reconciliation.
pub(crate) const LOG_IDENTITY_CAP: usize = 1000;

/// Total-line threshold that triggers compaction on ingest appends.
const COMPACT_AT: usize = 4 * LOG_IDENTITY_CAP;

/// Presentation retention (log-timeline-github-events): compaction
/// keeps this many of the NEWEST handled inbox records verbatim —
/// payload, identities, event time, and their ack records — so the
/// timeline surfaces don't lose the recent "event, then fix"
/// narrative to a threshold compaction. Older handled records fold to
/// identity observations as before.
pub(crate) const DISPLAY_CAP: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Transport {
    Poll,
    Relay,
}

impl Transport {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Transport::Poll => "poll",
            Transport::Relay => "relay",
        }
    }
}

/// The version this binary writes and fully understands. Bumps ONLY
/// for semantic changes to an existing `t` — additive optional fields
/// never bump it (event-log-format-compat).
const CURRENT_V: u64 = 1;

/// One parsed log line: a record this version owns, or a FOREIGN one
/// (unknown `t`, future `v`, or a known `t` whose fields drifted) —
/// preserved byte-verbatim, skipped for semantics, but its envelope
/// `seq` still reserves allocator namespace (codex 7e6e585).
#[derive(Debug, Clone)]
enum Line {
    Known(Box<Record>),
    Foreign { raw: String, seq: Option<u64> },
}

/// One JSONL line. `t`-tagged so a reader can never confuse the three
/// forms, whatever optional fields they share. `t`, `v`, and `seq`
/// are the VERSION-STABLE ENVELOPE: framing keys owned by every
/// version forever; all other keys are private to their `t`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum Record {
    /// Primary record of a PRESENTABLE event: the complete wake
    /// payload, re-presented byte-faithfully as backlog.
    Inbox {
        v: u64,
        seq: u64,
        at: u64,
        /// The event's github-side time (epoch), when the payload
        /// carried one — additive optional field, v stays 1
        /// (log-timeline-github-events). Timeline surfaces fall back
        /// to `at` when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_at: Option<u64>,
        /// BORN HANDLED (github-watch-resilience): a pre-watch event
        /// logged at baseline with its payload for the timeline —
        /// never backlog, never re-presented, no ack record needed.
        /// One atomic append per event, like every primary record.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        baseline: bool,
        transport: Transport,
        #[serde(skip_serializing_if = "Option::is_none")]
        feed_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        action_key: Option<String>,
        item: WaitItem,
    },
    /// Primary record of a NON-presentable observation: identity only.
    Obs {
        v: u64,
        at: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        feed_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        action_key: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        baseline: bool,
    },
    /// Secondary: marks an inbox seq handled. A PLAIN ack (no
    /// `ident`) covers every row of its seq; a DISCRIMINATED ack
    /// covers exactly the rows whose logical identity equals `ident`
    /// (wal-single-ingest-writer: damaged logs can hold two DIFFERENT
    /// events at one seq). Additive field — v stays 1.
    Ack {
        v: u64,
        seq: u64,
        at: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ident: Option<String>,
    },
}

impl Record {
    fn version(&self) -> u64 {
        match self {
            Record::Inbox { v, .. } | Record::Obs { v, .. } | Record::Ack { v, .. } => *v,
        }
    }
}

/// The CANONICAL logical identity of an inbox row — one definition
/// shared by fold, read_rows, the events resolver, and compaction
/// (wal-single-ingest-writer; no consumer may reimplement this
/// seq-keyed). Action-key first: that is the coordinator's
/// cross-transport contract (a poll copy carries both identities, its
/// relay twin only the key). Identity-less rows hash their COMPLETE
/// logical content — rows identical across all of it form one
/// indistinguishable occurrence group by construction.
pub(crate) fn logical_ident(
    feed_id: Option<&str>,
    action_key: Option<&str>,
    event_at: Option<u64>,
    transport: Transport,
    item: &WaitItem,
) -> String {
    if let Some(k) = action_key {
        return format!("k:{k}");
    }
    if let Some(f) = feed_id {
        return format!("f:{f}");
    }
    // blake3 over the serialized complete content: the discriminator
    // is a CORRECTNESS selector over user-controlled content, so it
    // must be collision-resistant — a collision would silently merge
    // and jointly acknowledge distinct events (codex 20cdd09).
    let content = serde_json::to_string(&(item, event_at, transport)).unwrap_or_default();
    let hex = blake3::hash(content.as_bytes()).to_hex();
    format!("h:{}", &hex.as_str()[..32])
}

/// The one ack-selector matcher: does this ack cover a row?
fn ack_covers(ack_seq: u64, ack_ident: Option<&str>, row_seq: u64, row_ident: &str) -> bool {
    ack_seq == row_seq && ack_ident.is_none_or(|i| i == row_ident)
}

/// Cache metadata sidecar — NEVER load-bearing for at-least-once.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct StateFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    poll_interval_floor: Option<u64>,
    saved_at: u64,
}

/// An unhandled inbox entry, ready to re-present.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InboxEntry {
    pub(crate) seq: u64,
    /// The row's canonical logical identity — takeover suppression is
    /// keyed on (seq, ident), never seq alone (codex 20cdd09).
    pub(crate) ident: String,
    pub(crate) item: WaitItem,
}

/// [`EventLog::read_rows`]'s result: the inbox rows plus how many
/// foreign records the read skipped.
#[derive(Debug)]
pub(crate) struct ReadRows {
    pub(crate) rows: Vec<EventRow>,
    pub(crate) foreign: usize,
}

/// One inbox entry with its lifecycle state — the `clank events`
/// inspector's row. Carries the record's durable identities
/// (feed id / action key): they're exactly the state one needs when
/// diagnosing poll/relay dedup or offline catch-up (codex c009500).
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct EventRow {
    pub(crate) seq: u64,
    pub(crate) at: u64,
    /// Github-side event time when known; display time is
    /// `event_at.unwrap_or(at)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) event_at: Option<u64>,
    pub(crate) transport: Transport,
    pub(crate) acked: bool,
    /// Pre-watch history logged at baseline (born handled).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) baseline: bool,
    /// The row's canonical logical identity ([`logical_ident`]) — the
    /// discriminator targeted acks use.
    pub(crate) ident: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) feed_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) action_key: Option<String>,
    pub(crate) item: WaitItem,
}

/// [`EventLog::fold`]'s output.
struct FoldOut {
    unhandled: Vec<InboxEntry>,
    feed_ids: Vec<String>,
    action_keys: Vec<String>,
    max_seq: u64,
    lines: usize,
    foreign: usize,
}

/// Everything a warm arm needs, rebuilt from the artifacts.
#[derive(Debug, Default)]
pub(crate) struct LoadedLog {
    /// Present iff the log file existed and parsed (a warm start —
    /// reconcile instead of baseline).
    pub(crate) warm: bool,
    /// Unhandled inbox entries in append (seq) order.
    pub(crate) unhandled: Vec<InboxEntry>,
    /// Newest-last feed ids from both primary forms, capped.
    pub(crate) feed_ids: Vec<String>,
    /// Newest-last action keys from both primary forms, capped.
    pub(crate) action_keys: Vec<String>,
    pub(crate) etag: Option<String>,
    pub(crate) poll_interval_floor: Option<u64>,
    /// The WAL was quarantined (interior corruption) — caller warns
    /// once and baselines fresh.
    pub(crate) log_quarantined: bool,
    /// `state.json` was corrupt — caller warns once; ONLY the cache
    /// resets, the WAL contents above are still authoritative.
    pub(crate) state_corrupt: bool,
    /// Well-formed records from another clank version, preserved but
    /// skipped — caller warns once with the count.
    pub(crate) foreign: usize,
}

/// flock RAII over the sidecar. Shared = append, exclusive = compact.
struct SidecarLock {
    _file: std::fs::File,
}

impl SidecarLock {
    fn acquire(path: &Path, exclusive: bool) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        let op = if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        };
        // SAFETY: valid owned fd; flock has no memory effects.
        let rc = unsafe { libc::flock(file.as_raw_fd(), op) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }
}
// flock releases with the fd on drop; no explicit LOCK_UN needed.

/// The per-source INGEST LEASE (wal-single-ingest-writer): exactly one
/// process may run a source's transports (polling AND the realtime
/// relay) and append its primary records. Exclusive, NON-blocking —
/// a second wait finding it held tails the WAL instead of ingesting.
/// flock dies with the process, so a killed holder frees the lease
/// with no cleanup protocol. Distinct from the append/compaction data
/// lock: this is about ingest exclusivity, not I/O atomicity.
pub(crate) struct IngestLease {
    _file: std::fs::File,
}

impl IngestLease {
    /// `Ok(None)` = CONTENDED (another live process holds the lease —
    /// strictly EWOULDBLOCK); any other failure is a real error the
    /// caller must handle distinctly (codex 8bf682e: an unopenable
    /// dir must not read as "someone else is ingesting" and strand a
    /// tailer forever).
    pub(crate) fn try_acquire(
        events_dir: &Path,
        source_key: &str,
    ) -> std::io::Result<Option<IngestLease>> {
        std::fs::create_dir_all(events_dir)?;
        let path = events_dir.join(format!("{source_key}.ingest.lock"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        // SAFETY: valid owned fd; flock has no memory effects.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(IngestLease { _file: file }));
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        Err(err)
    }
}

/// The ingest-side WAL handle shared between the poll loop and the
/// coordinator's emission gate (both live on one task; the Mutex is
/// never held across an await). Every write degrades WARN-ONCE on IO
/// error and the wake still emits: a broken disk must not silence the
/// agent — the log is the belt, the emission is the product. The
/// tradeoff is deliberate: a crash after a failed append re-delivers
/// nothing for that event, exactly the pre-WAL behavior.
#[derive(Clone)]
pub(crate) struct SharedWal(std::sync::Arc<std::sync::Mutex<WalInner>>);

struct WalInner {
    log: Option<EventLog>,
    warned: bool,
}

impl SharedWal {
    pub(crate) fn new(log: Option<EventLog>) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(WalInner {
            log,
            warned: false,
        })))
    }

    /// No-op sink — the coordinator's test constructor writes nowhere.
    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self::new(None)
    }

    fn with(&self, f: impl FnOnce(&mut EventLog) -> std::io::Result<()>) {
        let mut inner = self.0.lock().unwrap();
        let warned = inner.warned;
        if let Some(log) = inner.log.as_mut()
            && let Err(e) = f(log)
            && !warned
        {
            eprintln!("wait: github event log write failed ({e}); continuing without durability");
            inner.warned = true;
        }
    }

    /// The atomic durable decision for a presentable event — call
    /// BEFORE emitting.
    pub(crate) fn inbox(
        &self,
        transport: Transport,
        feed_id: Option<&str>,
        action_key: Option<&str>,
        event_at: Option<u64>,
        item: &WaitItem,
    ) {
        self.with(|log| {
            log.append_inbox(transport, feed_id, action_key, event_at, item)
                .map(|_| ())
        });
    }

    /// A pre-watch presentable event at baseline: payload kept for
    /// the timeline, born handled.
    pub(crate) fn baseline_inbox(
        &self,
        feed_id: Option<&str>,
        action_key: Option<&str>,
        event_at: Option<u64>,
        item: &WaitItem,
    ) {
        self.with(|log| {
            log.append_baseline_inbox(feed_id, action_key, event_at, item)
                .map(|_| ())
        });
    }

    /// The atomic durable decision for a non-presentable observation.
    pub(crate) fn obs(&self, feed_id: Option<&str>, action_key: Option<&str>, baseline: bool) {
        self.with(|log| log.append_observation(feed_id, action_key, baseline));
    }

    pub(crate) fn save_state(&self, etag: Option<&str>, poll_interval_floor: Option<u64>) {
        self.with(|log| log.save_state(etag, poll_interval_floor));
    }

    pub(crate) fn compact(&self) {
        let mut inner = self.0.lock().unwrap();
        let warned = inner.warned;
        if let Some(log) = inner.log.as_mut() {
            match log.maybe_compact() {
                Ok(0) => {}
                Ok(n) => eprintln!(
                    "wait: github event log compaction dropped {n} record(s) from another \
                     clank version (retention cap)"
                ),
                Err(e) => {
                    if !warned {
                        eprintln!(
                            "wait: github event log write failed ({e}); continuing without \
                             durability"
                        );
                        inner.warned = true;
                    }
                }
            }
        }
    }
}

/// Handle on one (agent, source) log. The ingest side holds one of
/// these for the life of the wait; the ack CLI opens its own — both
/// funnel every write through the sidecar lock.
pub(crate) struct EventLog {
    log_path: PathBuf,
    state_path: PathBuf,
    lock_path: PathBuf,
    /// `None` = the seq namespace is exhausted (a record reserved
    /// `u64::MAX`); inbox appends fail cleanly rather than wrapping.
    next_seq: Option<u64>,
    /// Lines believed in the file (loaded + appended since), for the
    /// compaction trigger. Approximate is fine: compaction re-reads
    /// under the exclusive lock.
    lines: usize,
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl EventLog {
    /// `source_key` must be filesystem-safe and stable for the
    /// source's identity (repo + config); the caller owns that
    /// derivation.
    pub(crate) fn open(events_dir: &Path, source_key: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(events_dir)?;
        Ok(Self {
            log_path: events_dir.join(format!("{source_key}.jsonl")),
            state_path: events_dir.join(format!("{source_key}.state.json")),
            lock_path: events_dir.join(format!("{source_key}.lock")),
            next_seq: Some(1),
            lines: 0,
        })
    }

    /// Parse the raw log text into the THREE-way classification
    /// (event-log-format-compat). `Err(())` = interior GARBAGE (not
    /// JSON objects); a torn final line (unparseable AND unterminated)
    /// is dropped as a normal crash artifact. Well-formed objects this
    /// version can't interpret are [`Line::Foreign`] — skipped for
    /// semantics, preserved for custody, and their envelope `seq`
    /// still reserves the allocator namespace.
    fn parse(text: &str) -> Result<Vec<Line>, ()> {
        let mut out = Vec::new();
        let lines: Vec<&str> = text.split('\n').collect();
        let ends_with_newline = text.ends_with('\n') || text.is_empty();
        let n = lines.len();
        for (i, line) in lines.iter().enumerate() {
            if line.is_empty() {
                continue;
            }
            let value = match serde_json::from_str::<serde_json::Value>(line) {
                Ok(v) if v.is_object() => v,
                // Only the physically-last, unterminated line may be
                // torn; any other non-object is real corruption.
                _ if i == n - 1 && !ends_with_newline => continue,
                _ => return Err(()),
            };
            // Known iff the kind parses fully AND its version is one
            // this binary understands. A recognized `t` whose fields
            // no longer parse is FOREIGN, not corruption — additive
            // evolution must never destroy a log.
            let known = serde_json::from_value::<Record>(value.clone())
                .ok()
                .filter(|r| r.version() <= CURRENT_V);
            match known {
                Some(r) => out.push(Line::Known(Box::new(r))),
                None => out.push(Line::Foreign {
                    raw: (*line).to_string(),
                    seq: value.get("seq").and_then(serde_json::Value::as_u64),
                }),
            }
        }
        Ok(out)
    }

    /// Fold parsed lines into load state. Foreign lines contribute
    /// exactly their envelope: the `seq` reservation and a count for
    /// the caller's one warning — never horizons or backlog.
    fn fold(lines_in: Vec<Line>) -> FoldOut {
        // All ack selectors up front — coverage is answered ONLY by
        // the shared matcher (wal-single-ingest-writer).
        let mut acks: Vec<(u64, Option<String>)> = Vec::new();
        for l in &lines_in {
            if let Line::Known(r) = l
                && let Record::Ack { seq, ident, .. } = r.as_ref()
            {
                acks.push((*seq, ident.clone()));
            }
        }
        // (seq, ident) is the first-class event key: physical rows
        // GROUP under it — identities union into the horizons
        // (whichever copy carried them), baseline aggregates over ALL
        // copies, and one live uncovered copy keeps the group open.
        // First-occurrence skipping would be append-order dependent
        // (codex 20cdd09: a baseline copy first must not hide a live
        // copy; a relay copy first must not lose its poll twin's
        // feed id).
        struct Group {
            seq: u64,
            ident: String,
            item: WaitItem,
            all_baseline: bool,
        }
        let mut groups: Vec<Group> = Vec::new();
        let mut by_key: std::collections::HashMap<(u64, String), usize> =
            std::collections::HashMap::new();
        let mut pushed_ids: std::collections::HashSet<(usize, String)> =
            std::collections::HashSet::new();
        let mut feed_ids = Vec::new();
        let mut action_keys = Vec::new();
        let mut max_seq = 0u64;
        let mut foreign = 0usize;
        let lines = lines_in.len();
        for l in lines_in {
            let r = match l {
                Line::Known(r) => *r,
                Line::Foreign { seq, .. } => {
                    foreign += 1;
                    if let Some(seq) = seq {
                        max_seq = max_seq.max(seq);
                    }
                    continue;
                }
            };
            match r {
                Record::Inbox {
                    seq,
                    feed_id,
                    action_key,
                    baseline,
                    event_at,
                    transport,
                    item,
                    ..
                } => {
                    max_seq = max_seq.max(seq);
                    let ident = logical_ident(
                        feed_id.as_deref(),
                        action_key.as_deref(),
                        event_at,
                        transport,
                        &item,
                    );
                    let gi = *by_key.entry((seq, ident.clone())).or_insert_with(|| {
                        groups.push(Group {
                            seq,
                            ident,
                            item,
                            all_baseline: true,
                        });
                        groups.len() - 1
                    });
                    groups[gi].all_baseline &= baseline;
                    // Union of durable identities, deduped per group,
                    // in encounter order.
                    if let Some(id) = feed_id
                        && pushed_ids.insert((gi, format!("f{id}")))
                    {
                        feed_ids.push(id);
                    }
                    if let Some(k) = action_key
                        && pushed_ids.insert((gi, format!("k{k}")))
                    {
                        action_keys.push(k);
                    }
                }
                Record::Obs {
                    feed_id,
                    action_key,
                    ..
                } => {
                    if let Some(id) = feed_id {
                        feed_ids.push(id);
                    }
                    if let Some(k) = action_key {
                        action_keys.push(k);
                    }
                }
                Record::Ack { .. } => {}
            }
        }
        let mut unhandled: Vec<InboxEntry> = groups
            .into_iter()
            .filter(|g| {
                let covered = acks
                    .iter()
                    .any(|(a, i)| ack_covers(*a, i.as_deref(), g.seq, &g.ident));
                !g.all_baseline && !covered
            })
            .map(|g| InboxEntry {
                seq: g.seq,
                ident: g.ident,
                item: g.item,
            })
            .collect();
        unhandled.sort_by_key(|e| e.seq);
        let cap_tail = |v: &mut Vec<String>| {
            if v.len() > LOG_IDENTITY_CAP {
                v.drain(..v.len() - LOG_IDENTITY_CAP);
            }
        };
        cap_tail(&mut feed_ids);
        cap_tail(&mut action_keys);
        FoldOut {
            unhandled,
            feed_ids,
            action_keys,
            max_seq,
            lines,
            foreign,
        }
    }

    /// Inspector read for the `clank events` CLI and the timeline
    /// layer: every inbox entry with its ack status, append order,
    /// plus the count of FOREIGN records skipped (the viewer's
    /// foreign-notice needs it — codex 92e9deb). READ-ONLY — unlike
    /// [`load`] it never quarantines (an inspector must not mutate the
    /// ingest side's artifacts); interior corruption is `Ok(None)` for
    /// the caller to report.
    pub(crate) fn read_rows(&self) -> std::io::Result<Option<ReadRows>> {
        let text = {
            let _lock = SidecarLock::acquire(&self.lock_path, false).ok();
            match std::fs::read_to_string(&self.log_path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Some(ReadRows {
                        rows: Vec::new(),
                        foreign: 0,
                    }));
                }
                Err(e) => return Err(e),
            }
        };
        let Ok(lines_in) = Self::parse(&text) else {
            return Ok(None);
        };
        let acks: Vec<(u64, Option<String>)> = lines_in
            .iter()
            .filter_map(|l| match l {
                Line::Known(r) => match r.as_ref() {
                    Record::Ack { seq, ident, .. } => Some((*seq, ident.clone())),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let foreign = lines_in
            .iter()
            .filter(|l| matches!(l, Line::Foreign { .. }))
            .count();
        // Grouped by the (seq, ident) event key like fold: one display
        // row per logical event, identities unioned (first Some wins),
        // baseline aggregated over all copies, acked only when
        // baseline-aggregate or matcher-covered (codex 20cdd09).
        let mut by_key: std::collections::HashMap<(u64, String), usize> =
            std::collections::HashMap::new();
        let mut rows: Vec<EventRow> = Vec::new();
        for l in lines_in {
            if let Line::Known(r) = l
                && let Record::Inbox {
                    seq,
                    at,
                    event_at,
                    baseline,
                    transport,
                    feed_id,
                    action_key,
                    item,
                    ..
                } = *r
            {
                let ident = logical_ident(
                    feed_id.as_deref(),
                    action_key.as_deref(),
                    event_at,
                    transport,
                    &item,
                );
                match by_key.entry((seq, ident.clone())) {
                    std::collections::hash_map::Entry::Vacant(v) => {
                        v.insert(rows.len());
                        rows.push(EventRow {
                            seq,
                            at,
                            event_at,
                            transport,
                            acked: false, // resolved below
                            baseline,
                            ident,
                            feed_id,
                            action_key,
                            item,
                        });
                    }
                    std::collections::hash_map::Entry::Occupied(o) => {
                        let row = &mut rows[*o.get()];
                        row.baseline &= baseline;
                        if row.feed_id.is_none() {
                            row.feed_id = feed_id;
                        }
                        if row.action_key.is_none() {
                            row.action_key = action_key;
                        }
                        if row.event_at.is_none() {
                            row.event_at = event_at;
                        }
                    }
                }
            }
        }
        for row in &mut rows {
            let covered = acks
                .iter()
                .any(|(a, i)| ack_covers(*a, i.as_deref(), row.seq, &row.ident));
            row.acked = row.baseline || covered;
        }
        rows.sort_by_key(|r| r.seq);
        Ok(Some(ReadRows { rows, foreign }))
    }

    /// Read both artifacts. Never fails: every failure mode maps to
    /// the plan's degradation (torn tail dropped, interior corruption
    /// quarantined to `<log>.corrupt`, corrupt state resets cache
    /// only).
    pub(crate) fn load(&mut self) -> LoadedLog {
        let mut out = LoadedLog::default();
        match std::fs::read_to_string(&self.state_path) {
            Ok(text) => match serde_json::from_str::<StateFile>(&text) {
                Ok(s) => {
                    out.etag = s.etag;
                    out.poll_interval_floor = s.poll_interval_floor;
                }
                Err(_) => out.state_corrupt = true,
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => out.state_corrupt = true,
        }
        let text = {
            let _lock = SidecarLock::acquire(&self.lock_path, false).ok();
            match std::fs::read_to_string(&self.log_path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return out,
                Err(_) => return out,
            }
        };
        match Self::parse(&text) {
            Ok(lines_in) => {
                let folded = Self::fold(lines_in);
                // Warm needs at least one KNOWN record: an all-foreign
                // log (a future format under this binary's custody)
                // must baseline, not reconcile against an empty cursor
                // and replay the feed's history as wakes.
                out.warm = folded.lines > folded.foreign;
                out.unhandled = folded.unhandled;
                out.feed_ids = folded.feed_ids;
                out.action_keys = folded.action_keys;
                out.foreign = folded.foreign;
                // The framing domain includes u64::MAX (a foreign
                // record may carry it) — exhaustion must be a clean
                // append error, never a wrap or panic (codex 777eee9).
                self.next_seq = folded.max_seq.checked_add(1);
                self.lines = folded.lines;
            }
            Err(()) => {
                // Quarantine preserves the evidence and stops the
                // corrupt file from re-tripping every arm. It MOVES
                // the log, so it needs the EXCLUSIVE lock — a
                // shared-lock appender must never be mid-write into
                // the inode being renamed (codex 66c8579). Re-parse
                // under the lock: the corruption verdict must be
                // about the bytes we're about to move, not a stale
                // read. If the lock can't be had, leave the file for
                // a later arm — baselining is safe either way.
                if let Ok(_lock) = SidecarLock::acquire(&self.lock_path, true)
                    && let Ok(text) = std::fs::read_to_string(&self.log_path)
                    && Self::parse(&text).is_err()
                {
                    let _ = std::fs::rename(
                        &self.log_path,
                        self.log_path.with_extension("jsonl.corrupt"),
                    );
                }
                out.log_quarantined = true;
            }
        }
        out
    }

    fn append(&mut self, record: &Record) -> std::io::Result<()> {
        let _lock = SidecarLock::acquire(&self.lock_path, false)?;
        // Open AFTER the lock: the fd must name the current inode,
        // never one a compactor just renamed away.
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
        line.push('\n');
        f.write_all(line.as_bytes())?;
        self.lines += 1;
        Ok(())
    }

    /// The single atomic durable decision for a presentable event —
    /// call BEFORE emitting the item.
    pub(crate) fn append_inbox(
        &mut self,
        transport: Transport,
        feed_id: Option<&str>,
        action_key: Option<&str>,
        event_at: Option<u64>,
        item: &WaitItem,
    ) -> std::io::Result<u64> {
        self.append_inbox_record(transport, feed_id, action_key, event_at, item, false)
    }

    /// A pre-watch event at baseline: full payload, BORN HANDLED —
    /// one atomic append, never backlog (github-watch-resilience).
    pub(crate) fn append_baseline_inbox(
        &mut self,
        feed_id: Option<&str>,
        action_key: Option<&str>,
        event_at: Option<u64>,
        item: &WaitItem,
    ) -> std::io::Result<u64> {
        self.append_inbox_record(Transport::Poll, feed_id, action_key, event_at, item, true)
    }

    fn append_inbox_record(
        &mut self,
        transport: Transport,
        feed_id: Option<&str>,
        action_key: Option<&str>,
        event_at: Option<u64>,
        item: &WaitItem,
        baseline: bool,
    ) -> std::io::Result<u64> {
        let Some(seq) = self.next_seq else {
            return Err(std::io::Error::other(
                "event-log seq namespace exhausted (a record reserves u64::MAX)",
            ));
        };
        self.append(&Record::Inbox {
            v: CURRENT_V,
            seq,
            at: now_epoch(),
            event_at,
            baseline,
            transport,
            feed_id: feed_id.map(str::to_owned),
            action_key: action_key.map(str::to_owned),
            item: item.clone(),
        })?;
        self.next_seq = seq.checked_add(1);
        Ok(seq)
    }

    /// The single atomic durable decision for a NON-presentable
    /// observation (filtered / own action / unclassifiable / deduped /
    /// baseline).
    pub(crate) fn append_observation(
        &mut self,
        feed_id: Option<&str>,
        action_key: Option<&str>,
        baseline: bool,
    ) -> std::io::Result<()> {
        self.append(&Record::Obs {
            v: CURRENT_V,
            at: now_epoch(),
            feed_id: feed_id.map(str::to_owned),
            action_key: action_key.map(str::to_owned),
            baseline,
        })
    }

    pub(crate) fn append_ack(&mut self, seq: u64) -> std::io::Result<()> {
        self.append_ack_selector(seq, None)
    }

    /// A DISCRIMINATED ack: covers only rows whose logical identity
    /// equals `ident` (damaged same-seq-distinct-events logs).
    pub(crate) fn append_ack_ident(&mut self, seq: u64, ident: &str) -> std::io::Result<()> {
        self.append_ack_selector(seq, Some(ident))
    }

    fn append_ack_selector(&mut self, seq: u64, ident: Option<&str>) -> std::io::Result<()> {
        self.append(&Record::Ack {
            v: CURRENT_V,
            seq,
            at: now_epoch(),
            ident: ident.map(str::to_owned),
        })
    }

    /// Atomic rewrite (tmp + rename); failed ticks simply don't call
    /// this.
    pub(crate) fn save_state(
        &self,
        etag: Option<&str>,
        poll_interval_floor: Option<u64>,
    ) -> std::io::Result<()> {
        let state = StateFile {
            etag: etag.map(str::to_owned),
            poll_interval_floor,
            saved_at: now_epoch(),
        };
        let tmp = self.state_path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec(&state).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(&tmp, &self.state_path)
    }

    /// Compact when the file has grown past [`COMPACT_AT`]: under the
    /// EXCLUSIVE sidecar lock, fold acked inbox records down to
    /// identity-only `obs` records, keep the newest
    /// [`LOG_IDENTITY_CAP`] identities, ALL unhandled inbox records
    /// (any age), and FOREIGN lines byte-verbatim (EXACTLY the newest
    /// [`LOG_IDENTITY_CAP`]; a dropped line's seq reservation dies
    /// with the identity it protected), then replace the file by
    /// rename. Appenders hold the lock shared and open after
    /// acquiring it, so nothing races the replacement. Returns the
    /// number of foreign lines dropped (caller reports).
    pub(crate) fn maybe_compact(&mut self) -> std::io::Result<usize> {
        if self.lines < COMPACT_AT {
            return Ok(0);
        }
        let _lock = SidecarLock::acquire(&self.lock_path, true)?;
        let text = match std::fs::read_to_string(&self.log_path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let Ok(lines_in) = Self::parse(&text) else {
            // Corruption is load's problem; don't compound it here.
            return Ok(0);
        };
        // Coverage is answered ONLY by the shared matcher over the
        // canonical identity — never seq-keyed (codex 77b5d97: a
        // discriminated ack against two colliding rows must handle
        // exactly its row, through rewrite and reload).
        let acks: Vec<(u64, Option<String>, Record)> = lines_in
            .iter()
            .filter_map(|l| match l {
                Line::Known(r) => match r.as_ref() {
                    Record::Ack { seq, ident, .. } => Some((*seq, ident.clone(), (**r).clone())),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        // Pass 1: GROUP physical rows by the (seq, ident) event key —
        // the same grouped state fold builds (codex 20cdd09). Every
        // physical row of a kept group is rewritten VERBATIM (the
        // read side re-groups), so no copy's identities are lost;
        // folded groups contribute the UNION of their copies'
        // identities.
        struct CGroup {
            seq: u64,
            ident: String,
            rows: Vec<Record>,
            all_baseline: bool,
        }
        let mut cgroups: Vec<CGroup> = Vec::new();
        let mut by_key: std::collections::HashMap<(u64, String), usize> =
            std::collections::HashMap::new();
        let mut identities: Vec<(Option<String>, Option<String>, u64, bool)> = Vec::new();
        let mut foreign: Vec<(String, Option<u64>)> = Vec::new();
        for l in lines_in {
            let r = match l {
                Line::Known(r) => *r,
                Line::Foreign { raw, seq } => {
                    foreign.push((raw, seq));
                    continue;
                }
            };
            match r {
                Record::Inbox {
                    seq,
                    ref feed_id,
                    ref action_key,
                    event_at,
                    transport,
                    baseline,
                    ref item,
                    ..
                } => {
                    let ident = logical_ident(
                        feed_id.as_deref(),
                        action_key.as_deref(),
                        event_at,
                        transport,
                        item,
                    );
                    let gi = *by_key.entry((seq, ident.clone())).or_insert_with(|| {
                        cgroups.push(CGroup {
                            seq,
                            ident,
                            rows: Vec::new(),
                            all_baseline: true,
                        });
                        cgroups.len() - 1
                    });
                    cgroups[gi].all_baseline &= baseline;
                    cgroups[gi].rows.push(r);
                }
                Record::Obs {
                    feed_id,
                    action_key,
                    at,
                    baseline,
                    ..
                } => {
                    if feed_id.is_some() || action_key.is_some() {
                        identities.push((feed_id, action_key, at, baseline));
                    }
                }
                Record::Ack { .. } => {}
            }
        }
        let covered = |g: &CGroup| {
            acks.iter()
                .any(|(a, i, _)| ack_covers(*a, i.as_deref(), g.seq, &g.ident))
        };
        let handled: Vec<usize> = (0..cgroups.len())
            .filter(|&i| cgroups[i].all_baseline || covered(&cgroups[i]))
            .collect();
        // Presentation split (log-timeline-github-events): the newest
        // DISPLAY_CAP handled GROUPS stay verbatim; older ones fold to
        // the union of their copies' identity observations.
        let fold_older = handled.len().saturating_sub(DISPLAY_CAP);
        let mut retained: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for (rank, &gi) in handled.iter().enumerate() {
            if rank < fold_older {
                for r in &cgroups[gi].rows {
                    if let Record::Inbox {
                        at,
                        feed_id,
                        action_key,
                        ..
                    } = r
                        && (feed_id.is_some() || action_key.is_some())
                    {
                        identities.push((feed_id.clone(), action_key.clone(), *at, false));
                    }
                }
            } else {
                retained.insert(gi);
            }
        }
        let handled_set: std::collections::HashSet<usize> = handled.iter().copied().collect();
        // Kept records: every physical row of unhandled groups and of
        // retained handled groups, in original group order.
        let mut unhandled: Vec<Record> = Vec::new();
        let mut retained_handled: Vec<(Record, String, bool)> = Vec::new();
        for (gi, g) in cgroups.iter().enumerate() {
            if !handled_set.contains(&gi) {
                unhandled.extend(g.rows.iter().cloned());
            } else if retained.contains(&gi) {
                for r in &g.rows {
                    retained_handled.push((r.clone(), g.ident.clone(), g.all_baseline));
                }
            }
        }
        // Ack retention through the SAME matcher: every ack that
        // covers a retained non-baseline handled group survives the
        // rewrite (deduped by selector); acks whose groups folded or
        // never existed are dropped.
        let mut kept_acks: Vec<Record> = Vec::new();
        let mut seen_selectors: std::collections::HashSet<(u64, Option<String>)> =
            std::collections::HashSet::new();
        for (a_seq, a_ident, rec) in &acks {
            if !seen_selectors.insert((*a_seq, a_ident.clone())) {
                continue;
            }
            let needed = cgroups.iter().enumerate().any(|(gi, g)| {
                retained.contains(&gi)
                    && !g.all_baseline
                    && ack_covers(*a_seq, a_ident.as_deref(), g.seq, &g.ident)
            });
            if needed {
                kept_acks.push(rec.clone());
            }
        }
        // Foreign retention: EXACTLY the newest LOG_IDENTITY_CAP
        // lines (codex 777eee9 — no immortal exemptions). Dropping a
        // line drops the identity its seq reservation protected, so
        // the reservation legitimately dies with it: the allocator
        // high-water is whatever the RETAINED lines say on the next
        // load.
        let start = foreign.len().saturating_sub(LOG_IDENTITY_CAP);
        let dropped_foreign = start;
        let kept_foreign: Vec<&(String, Option<u64>)> = foreign[start..].iter().collect();
        // The horizons are INDEPENDENT (codex 66c8579): a run of
        // relay-only action keys must not evict still-relevant feed
        // ids, nor vice versa. Keep the union of the newest
        // LOG_IDENTITY_CAP feed-id bearers and the newest
        // LOG_IDENTITY_CAP action-key bearers, in original order —
        // matching what fold() will reconstruct per horizon.
        let keep = {
            let mut keep = vec![false; identities.len()];
            let mut feed_seen = 0usize;
            let mut key_seen = 0usize;
            for (i, (feed_id, action_key, _, _)) in identities.iter().enumerate().rev() {
                let want_feed = feed_id.is_some() && feed_seen < LOG_IDENTITY_CAP;
                let want_key = action_key.is_some() && key_seen < LOG_IDENTITY_CAP;
                if want_feed || want_key {
                    keep[i] = true;
                    if feed_id.is_some() {
                        feed_seen += 1;
                    }
                    if action_key.is_some() {
                        key_seen += 1;
                    }
                }
            }
            keep
        };
        let mut out = String::new();
        for (i, (feed_id, action_key, at, baseline)) in identities.into_iter().enumerate() {
            if !keep[i] {
                continue;
            }
            let rec = Record::Obs {
                v: CURRENT_V,
                at,
                feed_id,
                action_key,
                baseline,
            };
            out.push_str(&serde_json::to_string(&rec).map_err(std::io::Error::other)?);
            out.push('\n');
        }
        let mut kept_lines = out.lines().count();
        for rec in &unhandled {
            out.push_str(&serde_json::to_string(rec).map_err(std::io::Error::other)?);
            out.push('\n');
            kept_lines += 1;
        }
        for (rec, _, _) in &retained_handled {
            out.push_str(&serde_json::to_string(rec).map_err(std::io::Error::other)?);
            out.push('\n');
            kept_lines += 1;
        }
        for ack in &kept_acks {
            out.push_str(&serde_json::to_string(ack).map_err(std::io::Error::other)?);
            out.push('\n');
            kept_lines += 1;
        }
        for (raw, _) in kept_foreign {
            out.push_str(raw);
            out.push('\n');
            kept_lines += 1;
        }
        let tmp = self.log_path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, out.as_bytes())?;
        std::fs::rename(&tmp, &self.log_path)?;
        self.lines = kept_lines;
        Ok(dropped_foreign)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(n: u64) -> WaitItem {
        WaitItem::GithubEvent {
            repo: "o/r".into(),
            event: "pr_comment".into(),
            detail: Some("review_comment".into()),
            number: Some(n),
            title: Some(format!("t{n}")),
            actor: Some("alice".into()),
            url: Some(format!("https://x/{n}")),
            instructions: None,
            content: None,
        }
    }

    fn log_in(dir: &Path) -> EventLog {
        EventLog::open(dir, "github-o-r-abcd").unwrap()
    }

    #[test]
    fn roundtrip_unhandled_cursor_and_keys() {
        let dir = tempdir();
        let mut log = log_in(&dir);
        assert!(!log.load().warm, "no file yet = cold start");
        log.append_observation(Some("100"), None, false).unwrap();
        let s1 = log
            .append_inbox(
                Transport::Poll,
                Some("101"),
                Some("review#1"),
                None,
                &item(1),
            )
            .unwrap();
        let s2 = log
            .append_inbox(Transport::Relay, None, Some("review#2"), None, &item(2))
            .unwrap();
        assert_ne!(s1, s2);
        let mut fresh = log_in(&dir);
        let loaded = fresh.load();
        assert!(loaded.warm);
        assert_eq!(
            loaded.unhandled.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![s1, s2]
        );
        assert_eq!(loaded.unhandled[0].item, item(1), "byte-faithful payload");
        assert_eq!(loaded.feed_ids, vec!["100".to_string(), "101".into()]);
        assert_eq!(
            loaded.action_keys,
            vec!["review#1".to_string(), "review#2".into()]
        );
    }

    #[test]
    fn ack_flips_unhandled_and_unknown_ack_is_inert() {
        let dir = tempdir();
        let mut log = log_in(&dir);
        let s1 = log
            .append_inbox(Transport::Poll, Some("1"), None, None, &item(1))
            .unwrap();
        log.append_inbox(Transport::Poll, Some("2"), None, None, &item(2))
            .unwrap();
        log.append_ack(s1).unwrap();
        log.append_ack(999).unwrap();
        let loaded = log_in(&dir).load();
        assert_eq!(loaded.unhandled.len(), 1);
        assert_eq!(loaded.unhandled[0].item, item(2));
        // Identities of acked entries remain in the cursor.
        assert_eq!(loaded.feed_ids, vec!["1".to_string(), "2".into()]);
    }

    #[test]
    fn seq_resumes_after_load() {
        let dir = tempdir();
        let mut log = log_in(&dir);
        let s1 = log
            .append_inbox(Transport::Poll, Some("1"), None, None, &item(1))
            .unwrap();
        let mut fresh = log_in(&dir);
        fresh.load();
        let s2 = fresh
            .append_inbox(Transport::Poll, Some("2"), None, None, &item(2))
            .unwrap();
        assert!(s2 > s1, "seq must not collide across restarts");
    }

    #[test]
    fn baseline_observations_seed_cursor_without_presenting() {
        let dir = tempdir();
        let mut log = log_in(&dir);
        for id in ["10", "11", "12"] {
            log.append_observation(Some(id), None, true).unwrap();
        }
        let loaded = log_in(&dir).load();
        assert!(loaded.warm);
        assert!(loaded.unhandled.is_empty());
        assert_eq!(loaded.feed_ids.len(), 3);
    }

    #[test]
    fn torn_final_line_is_dropped_not_corrupt() {
        let dir = tempdir();
        let mut log = log_in(&dir);
        log.append_inbox(Transport::Poll, Some("1"), None, None, &item(1))
            .unwrap();
        let path = dir.join("github-o-r-abcd.jsonl");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"t\":\"inbox\",\"seq\":2,\"at\":9,\"transp"); // torn write
        std::fs::write(&path, text).unwrap();
        let loaded = log_in(&dir).load();
        assert!(loaded.warm, "torn tail is a crash artifact, not corruption");
        assert!(!loaded.log_quarantined);
        assert_eq!(loaded.unhandled.len(), 1);
    }

    #[test]
    fn interior_corruption_quarantines_the_log() {
        let dir = tempdir();
        let mut log = log_in(&dir);
        log.append_inbox(Transport::Poll, Some("1"), None, None, &item(1))
            .unwrap();
        let path = dir.join("github-o-r-abcd.jsonl");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text = format!("not json at all\n{text}");
        std::fs::write(&path, text).unwrap();
        let loaded = log_in(&dir).load();
        assert!(loaded.log_quarantined);
        assert!(!loaded.warm, "quarantine degrades to baseline");
        assert!(loaded.unhandled.is_empty());
        assert!(dir.join("github-o-r-abcd.jsonl.corrupt").exists());
        assert!(!path.exists(), "quarantined file moved aside");
    }

    #[test]
    fn corrupt_state_json_keeps_wal_authoritative() {
        let dir = tempdir();
        let mut log = log_in(&dir);
        log.append_inbox(Transport::Poll, Some("1"), None, None, &item(1))
            .unwrap();
        log.save_state(Some("etag-x"), Some(60)).unwrap();
        std::fs::write(dir.join("github-o-r-abcd.state.json"), b"{broken").unwrap();
        let loaded = log_in(&dir).load();
        assert!(loaded.state_corrupt, "cache corruption is reported…");
        assert!(loaded.etag.is_none() && loaded.poll_interval_floor.is_none());
        assert!(loaded.warm, "…but the WAL stays authoritative");
        assert_eq!(loaded.unhandled.len(), 1);
        assert_eq!(loaded.feed_ids, vec!["1".to_string()]);
    }

    #[test]
    fn state_roundtrip() {
        let dir = tempdir();
        let log = log_in(&dir);
        log.save_state(Some("W/\"abc\""), Some(90)).unwrap();
        let loaded = log_in(&dir).load();
        assert_eq!(loaded.etag.as_deref(), Some("W/\"abc\""));
        assert_eq!(loaded.poll_interval_floor, Some(90));
        assert!(!loaded.state_corrupt);
    }

    #[test]
    fn compaction_preserves_unhandled_and_newest_identities() {
        let dir = tempdir();
        let mut log = log_in(&dir);
        let keep = log
            .append_inbox(
                Transport::Poll,
                Some("keep-me"),
                Some("k#1"),
                None,
                &item(7),
            )
            .unwrap();
        for i in 0..COMPACT_AT {
            let id = format!("{i}");
            let seq = log
                .append_inbox(Transport::Poll, Some(&id), None, None, &item(1))
                .unwrap();
            log.append_ack(seq).unwrap();
        }
        log.maybe_compact().unwrap();
        let loaded = log_in(&dir).load();
        assert_eq!(loaded.unhandled.len(), 1, "old unhandled survives any age");
        assert_eq!(loaded.unhandled[0].seq, keep);
        assert_eq!(loaded.unhandled[0].item, item(7));
        assert_eq!(loaded.feed_ids.len(), LOG_IDENTITY_CAP);
        let has = |id: &str| loaded.feed_ids.iter().any(|x| x == id);
        assert!(
            has(&format!("{}", COMPACT_AT - 1)) && has("keep-me"),
            "newest identities and unhandled identities survive"
        );
        assert!(!has("0"), "oldest identities are dropped");
        // The file physically shrank.
        let lines = std::fs::read_to_string(dir.join("github-o-r-abcd.jsonl"))
            .unwrap()
            .lines()
            .count();
        // Bound = identity horizon + DISPLAY_CAP retained handled
        // pairs (inbox + ack) + the unhandled row.
        assert!(
            lines <= LOG_IDENTITY_CAP + 2 * DISPLAY_CAP + 1,
            "compacted to bound, got {lines}"
        );
    }

    #[test]
    fn compaction_keeps_each_horizon_independently() {
        // codex 66c8579: a flood of newer relay-only action keys must
        // not evict the (few, older) feed ids — the horizons are
        // independent.
        let dir = tempdir();
        let mut log = log_in(&dir);
        for i in 0..5 {
            let id = format!("poll{i}");
            let seq = log
                .append_inbox(Transport::Poll, Some(&id), None, None, &item(1))
                .unwrap();
            log.append_ack(seq).unwrap();
        }
        for i in 0..COMPACT_AT {
            let key = format!("relay#{i}");
            let seq = log
                .append_inbox(Transport::Relay, None, Some(&key), None, &item(1))
                .unwrap();
            log.append_ack(seq).unwrap();
        }
        log.maybe_compact().unwrap();
        let loaded = log_in(&dir).load();
        for i in 0..5 {
            let id = format!("poll{i}");
            assert!(
                loaded.feed_ids.contains(&id),
                "feed id {id} evicted by newer relay-only keys"
            );
        }
        assert_eq!(loaded.action_keys.len(), LOG_IDENTITY_CAP);
        assert!(
            loaded
                .action_keys
                .contains(&format!("relay#{}", COMPACT_AT - 1)),
            "newest action keys kept"
        );
    }

    #[test]
    fn append_during_compaction_loses_nothing() {
        let dir = tempdir();
        let mut log = log_in(&dir);
        for i in 0..COMPACT_AT {
            let id = format!("pre{i}");
            let seq = log
                .append_inbox(Transport::Poll, Some(&id), None, None, &item(1))
                .unwrap();
            log.append_ack(seq).unwrap();
        }
        let dir2 = dir.clone();
        let appender = std::thread::spawn(move || {
            let mut log = log_in(&dir2);
            log.load();
            let mut seqs = Vec::new();
            for i in 0..200u64 {
                let key = format!("race#{i}");
                seqs.push(
                    log.append_inbox(Transport::Relay, None, Some(&key), None, &item(i))
                        .unwrap(),
                );
            }
            seqs
        });
        // Compact repeatedly while the appender runs.
        for _ in 0..20 {
            log.lines = COMPACT_AT; // force the trigger each round
            log.maybe_compact().unwrap();
        }
        let seqs = appender.join().unwrap();
        let loaded = log_in(&dir).load();
        let unhandled: std::collections::HashSet<u64> =
            loaded.unhandled.iter().map(|e| e.seq).collect();
        for seq in seqs {
            assert!(
                unhandled.contains(&seq),
                "appended entry {seq} lost by a racing compaction"
            );
        }
    }

    /// codex df3f1a7: every crash prefix leaves a presentable event
    /// either unhandled in the WAL or absent from the durable cursor —
    /// never suppressed. Truncate the real file at every byte
    /// boundary and check the invariant at each.
    #[test]
    fn crash_prefix_never_suppresses_presentable() {
        let dir = tempdir();
        let mut log = log_in(&dir);
        log.append_observation(Some("before"), None, false).unwrap();
        log.append_inbox(Transport::Poll, Some("target"), Some("k#t"), None, &item(1))
            .unwrap();
        let path = dir.join("github-o-r-abcd.jsonl");
        let full = std::fs::read(&path).unwrap();
        for cut in 0..=full.len() {
            std::fs::write(&path, &full[..cut]).unwrap();
            let loaded = log_in(&dir).load();
            let in_cursor = loaded.feed_ids.iter().any(|id| id == "target");
            let unhandled = loaded.unhandled.iter().any(|e| {
                matches!(
                    &e.item,
                    WaitItem::GithubEvent {
                        number: Some(1),
                        ..
                    }
                )
            });
            assert!(
                !in_cursor || unhandled,
                "prefix {cut}/{} suppressed the event (cursor={in_cursor}, unhandled={unhandled})",
                full.len()
            );
        }
    }

    // ── damaged logs: the shared identity model (wal-single-ingest-writer) ──

    /// Append a raw duplicate of an existing line (the duplicate-
    /// writer field shape: same seq re-allocated by a second process).
    fn duplicate_line_with(dir: &Path, replace: &[(&str, &str)]) {
        let path = dir.join("github-o-r-abcd.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let mut line = text.lines().last().unwrap().to_string();
        for (from, to) in replace {
            line = line.replace(from, to);
        }
        std::fs::write(&path, format!("{text}{line}\n")).unwrap();
    }

    #[test]
    fn field_shape_same_seq_same_event_collapses_and_plain_acks() {
        // The fsctl report: two inbox rows, same seq, same feed_id.
        let dir = tempdir();
        let mut log = log_in(&dir);
        log.append_inbox(Transport::Poll, Some("f-522"), None, None, &item(522))
            .unwrap();
        duplicate_line_with(&dir, &[]);
        let loaded = log_in(&dir).load();
        assert_eq!(loaded.unhandled.len(), 1, "one logical event");
        let rows = log_in(&dir).read_rows().unwrap().unwrap().rows;
        assert_eq!(rows.len(), 1, "one display row");
        // The manual-workaround shape: one PLAIN ack clears it.
        let mut log = log_in(&dir);
        log.load();
        log.append_ack(1).unwrap();
        assert!(log_in(&dir).load().unhandled.is_empty());
    }

    #[test]
    fn distinct_events_at_one_seq_stay_addressable_across_compaction() {
        // codex 0fb262b/77b5d97: same seq, DIFFERENT feed events —
        // nothing may be silently erased, a discriminated ack clears
        // exactly one, and the un-acked row survives a compaction.
        let dir = tempdir();
        let mut log = log_in(&dir);
        log.append_inbox(Transport::Poll, Some("A"), None, None, &item(1))
            .unwrap();
        duplicate_line_with(
            &dir,
            &[("\"A\"", "\"B\""), ("\"number\":1", "\"number\":2")],
        );
        let loaded = log_in(&dir).load();
        assert_eq!(loaded.unhandled.len(), 2, "both real events present");
        let rows = log_in(&dir).read_rows().unwrap().unwrap().rows;
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].ident, rows[1].ident);
        // Discriminated ack for the "A" row only.
        let ident_a = rows
            .iter()
            .find(|r| r.feed_id.as_deref() == Some("A"))
            .unwrap();
        let mut log = log_in(&dir);
        log.load();
        log.append_ack_ident(1, &ident_a.ident.clone()).unwrap();
        let loaded = log_in(&dir).load();
        assert_eq!(loaded.unhandled.len(), 1, "only B remains open");
        assert!(matches!(
            &loaded.unhandled[0].item,
            WaitItem::GithubEvent {
                number: Some(2),
                ..
            }
        ));
        // Across a forced compaction + reload: the acked row is
        // handled (retained or folded), B is STILL unhandled.
        let mut log = log_in(&dir);
        log.load();
        log.lines = COMPACT_AT;
        log.maybe_compact().unwrap();
        let loaded = log_in(&dir).load();
        assert_eq!(loaded.unhandled.len(), 1, "compaction must not erase B");
        assert!(matches!(
            &loaded.unhandled[0].item,
            WaitItem::GithubEvent {
                number: Some(2),
                ..
            }
        ));
        let rows = log_in(&dir).read_rows().unwrap().unwrap().rows;
        let a = rows.iter().find(|r| r.feed_id.as_deref() == Some("A"));
        if let Some(a) = a {
            assert!(a.acked, "A's discriminated ack survived the rewrite");
        }
    }

    #[test]
    fn identity_less_identical_rows_group_and_poll_relay_pair_collapses() {
        let dir = tempdir();
        let mut log = log_in(&dir);
        // Keyless, no feed id — identical complete content twice.
        log.append_inbox(Transport::Poll, None, None, Some(9), &item(3))
            .unwrap();
        duplicate_line_with(&dir, &[]);
        // The damaged poll/relay pair at one seq: poll row carries
        // feed_id + action key; the relay copy only the same key.
        log_in(&dir).load(); // no-op read
        let mut log = log_in(&dir);
        log.load();
        log.append_inbox(Transport::Poll, Some("F"), Some("k#9"), None, &item(4))
            .unwrap();
        duplicate_line_with(
            &dir,
            &[
                ("\"transport\":\"poll\"", "\"transport\":\"relay\""),
                ("\"feed_id\":\"F\",", ""),
            ],
        );
        let loaded = log_in(&dir).load();
        // Keyless identical pair grouped; poll/relay pair collapsed
        // under action-key-first identity → exactly two logical
        // events.
        assert_eq!(loaded.unhandled.len(), 2, "{:?}", loaded.unhandled);
        let rows = log_in(&dir).read_rows().unwrap().unwrap().rows;
        assert_eq!(rows.len(), 2);
        // The keyless group's hash discriminator clears the group.
        let keyless = rows.iter().find(|r| r.ident.starts_with("h:")).unwrap();
        let (kseq, kident) = (keyless.seq, keyless.ident.clone());
        let mut log = log_in(&dir);
        log.load();
        log.append_ack_ident(kseq, &kident).unwrap();
        assert_eq!(log_in(&dir).load().unhandled.len(), 1);
    }

    #[test]
    fn ack_appends_while_the_ingest_lease_is_held() {
        // The ingest lease gates TRANSPORTS, not the data path: the
        // ack CLI must append normally while a wait holds the lease
        // (codex b649c72).
        let dir = tempdir();
        let lease = IngestLease::try_acquire(&dir, "github-o-r-abcd")
            .unwrap()
            .unwrap();
        let mut log = log_in(&dir);
        let seq = log
            .append_inbox(Transport::Poll, Some("1"), None, None, &item(1))
            .unwrap();
        let mut acker = log_in(&dir);
        acker.load();
        acker.append_ack(seq).unwrap();
        assert!(log_in(&dir).load().unhandled.is_empty());
        drop(lease);
    }

    #[test]
    fn baseline_copy_first_never_hides_a_live_copy() {
        // Append-order independence (codex 20cdd09/b649c72): the
        // BASELINE copy lands first, the live copy second — the group
        // must aggregate to LIVE-unhandled, not inherit the first
        // row's born-handled state.
        let dir = tempdir();
        let mut log = log_in(&dir);
        log.append_baseline_inbox(Some("X"), None, Some(5), &item(9))
            .unwrap();
        // A live duplicate of the same logical event at the same seq
        // (duplicate-writer damage): flip baseline off on the raw
        // line.
        duplicate_line_with(&dir, &[("\"baseline\":true,", "")]);
        let loaded = log_in(&dir).load();
        assert_eq!(
            loaded.unhandled.len(),
            1,
            "a live copy keeps the group open regardless of order: {loaded:?}"
        );
        let rows = log_in(&dir).read_rows().unwrap().unwrap().rows;
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].baseline, "baseline aggregates over ALL copies");
        assert!(!rows[0].acked);
    }

    #[test]
    fn relay_copy_first_keeps_the_poll_twins_feed_id_across_compaction() {
        // The relay copy (action key only) lands BEFORE its poll twin
        // (feed id + key): the feed id must reach the cursor horizon
        // and survive compaction via the group's identity union
        // (codex 20cdd09/b649c72).
        let dir = tempdir();
        let mut log = log_in(&dir);
        log.append_inbox(Transport::Relay, None, Some("k#7"), None, &item(7))
            .unwrap();
        // The poll twin at the SAME seq (duplicate-writer damage).
        let path = dir.join("github-o-r-abcd.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let line = text
            .lines()
            .last()
            .unwrap()
            .replace("\"transport\":\"relay\"", "\"transport\":\"poll\"")
            .replace(
                "\"action_key\":\"k#7\"",
                "\"feed_id\":\"F7\",\"action_key\":\"k#7\"",
            );
        std::fs::write(&path, format!("{text}{line}\n")).unwrap();
        let loaded = log_in(&dir).load();
        assert_eq!(loaded.unhandled.len(), 1, "one logical event");
        assert!(
            loaded.feed_ids.contains(&"F7".to_string()),
            "the poll twin's feed id reaches the horizon: {:?}",
            loaded.feed_ids
        );
        // Ack it, force a compaction, reload: the identity union
        // (feed id AND key) survives the rewrite.
        let mut log = log_in(&dir);
        log.load();
        log.append_ack(1).unwrap();
        log.lines = COMPACT_AT;
        log.maybe_compact().unwrap();
        let loaded = log_in(&dir).load();
        assert!(loaded.unhandled.is_empty());
        assert!(
            loaded.feed_ids.contains(&"F7".to_string()),
            "feed id survives compaction: {:?}",
            loaded.feed_ids
        );
        assert!(loaded.action_keys.contains(&"k#7".to_string()));
    }

    #[test]
    fn baseline_inbox_is_born_handled_with_payload() {
        // github-watch-resilience: pre-watch history keeps its payload
        // for the timeline but never becomes backlog — one atomic
        // append, no ack record, warm cursor.
        let dir = tempdir();
        let mut log = log_in(&dir);
        log.append_baseline_inbox(Some("10"), Some("k#1"), Some(500), &item(1))
            .unwrap();
        let loaded = log_in(&dir).load();
        assert!(loaded.warm);
        assert!(loaded.unhandled.is_empty(), "born handled: never backlog");
        assert_eq!(loaded.feed_ids, vec!["10".to_string()], "cursor seeded");
        assert_eq!(loaded.action_keys, vec!["k#1".to_string()]);
        let rows = log_in(&dir).read_rows().unwrap().unwrap().rows;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].acked && rows[0].baseline);
        assert_eq!(rows[0].event_at, Some(500));
        assert_eq!(rows[0].item, item(1), "payload preserved for display");
        // Crash-prefix: truncate at every byte — a baseline record can
        // never surface as presentable-unhandled.
        let path = dir.join("github-o-r-abcd.jsonl");
        let full = std::fs::read(&path).unwrap();
        for cut in 0..=full.len() {
            std::fs::write(&path, &full[..cut]).unwrap();
            let l = log_in(&dir).load();
            assert!(
                l.unhandled.is_empty(),
                "prefix {cut} made baseline history presentable"
            );
        }
    }

    #[test]
    fn baseline_inbox_ages_through_display_retention_without_acks() {
        let dir = tempdir();
        let mut log = log_in(&dir);
        for i in 0..COMPACT_AT {
            let id = format!("{i}");
            log.append_baseline_inbox(Some(&id), None, Some(i as u64), &item(i as u64))
                .unwrap();
        }
        log.maybe_compact().unwrap();
        let rows = log_in(&dir).read_rows().unwrap().unwrap().rows;
        assert_eq!(rows.len(), DISPLAY_CAP, "handled retention applies");
        assert!(rows.iter().all(|r| r.baseline && r.acked));
        let text = std::fs::read_to_string(dir.join("github-o-r-abcd.jsonl")).unwrap();
        assert!(
            !text.contains("\"t\":\"ack\""),
            "born-handled needs no ack records"
        );
        // Reload: still no backlog, horizons intact.
        let loaded = log_in(&dir).load();
        assert!(loaded.unhandled.is_empty());
        assert_eq!(loaded.feed_ids.len(), LOG_IDENTITY_CAP);
    }

    #[test]
    fn compaction_retains_recent_handled_for_display_with_ack_state() {
        // log-timeline-github-events: the newest DISPLAY_CAP handled
        // records survive compaction VERBATIM with their acks (no
        // re-presentation after reload); older handled fold to
        // identity obs; unhandled stays lossless; horizons intact.
        let dir = tempdir();
        let mut log = log_in(&dir);
        let total = COMPACT_AT;
        let unhandled_seq = log
            .append_inbox(
                Transport::Poll,
                Some("keep-unhandled"),
                None,
                None,
                &item(1),
            )
            .unwrap();
        for i in 0..total {
            let id = format!("{i}");
            let seq = log
                .append_inbox(
                    Transport::Poll,
                    Some(&id),
                    Some(&format!("k#{i}")),
                    Some(1_000_000 + i as u64),
                    &item(i as u64),
                )
                .unwrap();
            log.append_ack(seq).unwrap();
        }
        log.maybe_compact().unwrap();

        let rows = log_in(&dir).read_rows().unwrap().unwrap().rows;
        let handled: Vec<_> = rows.iter().filter(|r| r.acked).collect();
        assert_eq!(
            handled.len(),
            DISPLAY_CAP,
            "exactly the display bound of handled rows survives"
        );
        // The newest ones, payload + event time intact.
        let newest = handled.last().unwrap();
        assert_eq!(newest.item, item((total - 1) as u64));
        assert_eq!(newest.event_at, Some(1_000_000 + (total as u64) - 1));
        let oldest_retained = handled.first().unwrap();
        assert_eq!(
            oldest_retained.item,
            item((total - DISPLAY_CAP) as u64),
            "age-out is exactly at the bound"
        );
        // Ack state survives: reload re-presents ONLY the unhandled row.
        let loaded = log_in(&dir).load();
        assert_eq!(loaded.unhandled.len(), 1);
        assert_eq!(loaded.unhandled[0].seq, unhandled_seq);
        // Horizons unaffected by retention: newest CAP identities.
        assert_eq!(loaded.feed_ids.len(), LOG_IDENTITY_CAP);
        assert!(loaded.feed_ids.contains(&format!("{}", total - 1)));
        assert_eq!(loaded.action_keys.len(), LOG_IDENTITY_CAP);
    }

    #[test]
    fn foreign_records_skip_but_preserve_and_reserve_seq() {
        // event-log-format-compat: unknown t, future v, and a known t
        // with drifted fields are FOREIGN — no horizons, no backlog,
        // preserved verbatim, seq reserved (codex 7e6e585).
        let dir = tempdir();
        let mut log = log_in(&dir);
        log.append_inbox(Transport::Poll, Some("1"), None, None, &item(1))
            .unwrap();
        let path = dir.join("github-o-r-abcd.jsonl");
        let unknown_kind = r#"{"t":"tombstone","v":1,"seq":90,"reason":"future"}"#;
        let future_v = r#"{"t":"inbox","v":99,"seq":42,"payload":{"shape":"unknowable"}}"#;
        let drifted = r#"{"t":"ack","v":1,"seq":"not-a-number"}"#;
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(&format!("{unknown_kind}\n{future_v}\n{drifted}\n"));
        std::fs::write(&path, &text).unwrap();

        let mut fresh = log_in(&dir);
        let loaded = fresh.load();
        assert!(loaded.warm, "foreign lines don't cost the warm start");
        assert!(!loaded.log_quarantined, "foreign is NOT corruption");
        assert_eq!(loaded.foreign, 3);
        assert_eq!(loaded.unhandled.len(), 1, "backlog untouched");
        assert_eq!(loaded.feed_ids, vec!["1".to_string()], "horizons untouched");
        // The allocator reserves ABOVE the highest foreign seq…
        let seq = fresh
            .append_inbox(Transport::Poll, Some("2"), None, None, &item(2))
            .unwrap();
        assert_eq!(seq, 91, "foreign seq 90 reserved the namespace");
        // …and the foreign lines survive the append round-trip
        // byte-verbatim.
        let text = std::fs::read_to_string(&path).unwrap();
        for line in [unknown_kind, future_v, drifted] {
            assert!(text.contains(line), "foreign line lost: {line}");
        }
    }

    #[test]
    fn foreign_lines_survive_compaction_and_keep_the_reservation() {
        let dir = tempdir();
        let mut log = log_in(&dir);
        for i in 0..COMPACT_AT {
            let id = format!("{i}");
            let seq = log
                .append_inbox(Transport::Poll, Some(&id), None, None, &item(1))
                .unwrap();
            log.append_ack(seq).unwrap();
        }
        let path = dir.join("github-o-r-abcd.jsonl");
        let high = r#"{"t":"inbox","v":99,"seq":900000,"payload":true}"#;
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(&format!("{high}\n"));
        std::fs::write(&path, &text).unwrap();

        let mut fresh = log_in(&dir);
        fresh.load();
        fresh.lines = COMPACT_AT; // force the trigger
        fresh.maybe_compact().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(high),
            "reservation-holding line compacted away"
        );
        let mut reloaded = log_in(&dir);
        reloaded.load();
        let seq = reloaded
            .append_inbox(Transport::Poll, Some("x"), None, None, &item(9))
            .unwrap();
        assert_eq!(seq, 900_001, "no seq reuse after compaction + reload");
    }

    #[test]
    fn foreign_custody_cap_is_exact_and_reservations_die_with_dropped_lines() {
        // codex 777eee9: EXACTLY the newest cap foreign lines survive
        // — an older high-seq line is NOT immortal; its reservation
        // dies with it, and reload allocates above the highest
        // RETAINED seq.
        let dir = tempdir();
        let mut log = log_in(&dir);
        for i in 0..COMPACT_AT {
            let id = format!("{i}");
            let seq = log
                .append_inbox(Transport::Poll, Some(&id), None, None, &item(1))
                .unwrap();
            log.append_ack(seq).unwrap();
        }
        let path = dir.join("github-o-r-abcd.jsonl");
        let mut text = std::fs::read_to_string(&path).unwrap();
        // OLDEST foreign line holds a huge seq; then >cap newer ones.
        text.push_str("{\"t\":\"future\",\"v\":9,\"seq\":800000}\n");
        let extra = 5;
        for i in 0..(LOG_IDENTITY_CAP + extra) {
            text.push_str(&format!(
                "{{\"t\":\"future\",\"v\":9,\"seq\":{}}}\n",
                10_000 + i
            ));
        }
        std::fs::write(&path, &text).unwrap();

        let mut fresh = log_in(&dir);
        fresh.load();
        fresh.lines = COMPACT_AT; // force the trigger
        let dropped = fresh.maybe_compact().unwrap();
        assert_eq!(dropped, extra + 1, "exact cap: oldest lines drop, counted");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("800000"),
            "an old high-seq foreign line must not be immortal"
        );
        let foreign_lines = text.lines().filter(|l| l.contains("future")).count();
        assert_eq!(foreign_lines, LOG_IDENTITY_CAP);

        let mut reloaded = log_in(&dir);
        reloaded.load();
        let seq = reloaded
            .append_inbox(Transport::Poll, Some("x"), None, None, &item(9))
            .unwrap();
        let max_retained = 10_000 + (LOG_IDENTITY_CAP + extra - 1) as u64;
        assert_eq!(
            seq,
            max_retained + 1,
            "allocates above the highest RETAINED reservation"
        );
    }

    #[test]
    fn seq_exhaustion_fails_cleanly_never_wraps() {
        // codex 777eee9: u64::MAX is in the accepted framing domain.
        let dir = tempdir();
        let path = dir.join("github-o-r-abcd.jsonl");
        std::fs::write(
            &path,
            format!("{{\"t\":\"future\",\"v\":9,\"seq\":{}}}\n", u64::MAX),
        )
        .unwrap();
        let mut log = log_in(&dir);
        let loaded = log.load();
        assert_eq!(loaded.foreign, 1);
        let err = log
            .append_inbox(Transport::Poll, Some("1"), None, None, &item(1))
            .expect_err("exhausted namespace must error, not wrap");
        assert!(err.to_string().contains("exhausted"), "{err}");
        // Observations and acks (no allocation) still work.
        log.append_observation(Some("2"), None, false).unwrap();
    }

    #[test]
    fn all_foreign_log_is_not_warm() {
        // A future format under this binary's custody must baseline —
        // a warm empty cursor would replay the feed's history.
        let dir = tempdir();
        let path = dir.join("github-o-r-abcd.jsonl");
        std::fs::write(&path, "{\"t\":\"inbox\",\"v\":99,\"seq\":7}\n").unwrap();
        let loaded = log_in(&dir).load();
        assert!(!loaded.warm);
        assert!(!loaded.log_quarantined);
        assert_eq!(loaded.foreign, 1);
    }

    #[test]
    fn new_records_stamp_v1_and_garbage_rules_hold() {
        let dir = tempdir();
        let mut log = log_in(&dir);
        log.append_inbox(Transport::Poll, Some("1"), None, None, &item(1))
            .unwrap();
        log.append_observation(Some("2"), None, false).unwrap();
        log.append_ack(1).unwrap();
        let text = std::fs::read_to_string(dir.join("github-o-r-abcd.jsonl")).unwrap();
        for line in text.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["v"], 1, "every written record carries v: {line}");
        }
        // Interior non-JSON is still corruption, not foreign.
        let path = dir.join("github-o-r-abcd.jsonl");
        std::fs::write(&path, format!("plain garbage\n{text}")).unwrap();
        let loaded = log_in(&dir).load();
        assert!(loaded.log_quarantined);
    }

    fn tempdir() -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "clank-gh-event-log-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
