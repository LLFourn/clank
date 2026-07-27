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
    /// Secondary: marks an inbox seq handled.
    Ack { v: u64, seq: u64, at: u64 },
}

impl Record {
    fn version(&self) -> u64 {
        match self {
            Record::Inbox { v, .. } | Record::Obs { v, .. } | Record::Ack { v, .. } => *v,
        }
    }
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
        let mut acked = std::collections::HashSet::new();
        for l in &lines_in {
            if let Line::Known(r) = l
                && let Record::Ack { seq, .. } = r.as_ref()
            {
                acked.insert(*seq);
            }
        }
        let mut unhandled = Vec::new();
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
                    item,
                    ..
                } => {
                    max_seq = max_seq.max(seq);
                    if let Some(id) = feed_id {
                        feed_ids.push(id);
                    }
                    if let Some(k) = action_key {
                        action_keys.push(k);
                    }
                    if !acked.contains(&seq) {
                        unhandled.push(InboxEntry { seq, item });
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
        let acked: std::collections::HashSet<u64> = lines_in
            .iter()
            .filter_map(|l| match l {
                Line::Known(r) => match r.as_ref() {
                    Record::Ack { seq, .. } => Some(*seq),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let foreign = lines_in
            .iter()
            .filter(|l| matches!(l, Line::Foreign { .. }))
            .count();
        let mut rows = Vec::new();
        for l in lines_in {
            if let Line::Known(r) = l
                && let Record::Inbox {
                    seq,
                    at,
                    event_at,
                    transport,
                    feed_id,
                    action_key,
                    item,
                    ..
                } = *r
            {
                rows.push(EventRow {
                    seq,
                    at,
                    event_at,
                    transport,
                    acked: acked.contains(&seq),
                    feed_id,
                    action_key,
                    item,
                });
            }
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
        self.append(&Record::Ack {
            v: CURRENT_V,
            seq,
            at: now_epoch(),
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
        let acked: std::collections::HashSet<u64> = lines_in
            .iter()
            .filter_map(|l| match l {
                Line::Known(r) => match r.as_ref() {
                    Record::Ack { seq, .. } => Some(*seq),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        // Identity-bearing records, oldest→newest, folded to (feed_id,
        // action_key, at, baseline) — keep only the newest cap.
        // Handled inbox records split at the presentation bound
        // (log-timeline-github-events): the newest DISPLAY_CAP stay
        // VERBATIM — with their ack records, or a reload would
        // re-present them as unhandled — while older ones fold to
        // identity observations. Two passes: count acked inbox first
        // so the split point is known when walking in order.
        let acked_inbox_total = lines_in
            .iter()
            .filter(|l| {
                matches!(l, Line::Known(r)
                    if matches!(r.as_ref(), Record::Inbox { seq, .. } if acked.contains(seq)))
            })
            .count();
        let fold_older = acked_inbox_total.saturating_sub(DISPLAY_CAP);
        let mut acked_seen = 0usize;
        let mut identities: Vec<(Option<String>, Option<String>, u64, bool)> = Vec::new();
        let mut unhandled: Vec<Record> = Vec::new();
        let mut retained_handled: Vec<Record> = Vec::new();
        let mut ack_records: std::collections::HashMap<u64, Record> =
            std::collections::HashMap::new();
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
                    at,
                    ref feed_id,
                    ref action_key,
                    ..
                } if acked.contains(&seq) => {
                    acked_seen += 1;
                    if acked_seen <= fold_older {
                        if feed_id.is_some() || action_key.is_some() {
                            identities.push((feed_id.clone(), action_key.clone(), at, false));
                        }
                    } else {
                        retained_handled.push(r);
                    }
                }
                Record::Inbox { .. } => unhandled.push(r),
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
                Record::Ack { seq, .. } => {
                    // Keep the FIRST ack per seq for retained pairs.
                    ack_records.entry(seq).or_insert(r);
                }
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
        for rec in &retained_handled {
            out.push_str(&serde_json::to_string(rec).map_err(std::io::Error::other)?);
            out.push('\n');
            kept_lines += 1;
            let Record::Inbox { seq, .. } = rec else {
                unreachable!("retained_handled holds only inbox records");
            };
            if let Some(ack) = ack_records.get(seq) {
                out.push_str(&serde_json::to_string(ack).map_err(std::io::Error::other)?);
                out.push('\n');
                kept_lines += 1;
            }
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
