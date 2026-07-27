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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Transport {
    Poll,
    Relay,
}

/// One JSONL line. `t`-tagged so a reader can never confuse the three
/// forms, whatever optional fields they share.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum Record {
    /// Primary record of a PRESENTABLE event: the complete wake
    /// payload, re-presented byte-faithfully as backlog.
    Inbox {
        seq: u64,
        at: u64,
        transport: Transport,
        #[serde(skip_serializing_if = "Option::is_none")]
        feed_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        action_key: Option<String>,
        item: WaitItem,
    },
    /// Primary record of a NON-presentable observation: identity only.
    Obs {
        at: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        feed_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        action_key: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        baseline: bool,
    },
    /// Secondary: marks an inbox seq handled.
    Ack { seq: u64, at: u64 },
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

/// One inbox entry with its lifecycle state — the `clank events`
/// inspector's row. Carries the record's durable identities
/// (feed id / action key): they're exactly the state one needs when
/// diagnosing poll/relay dedup or offline catch-up (codex c009500).
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct EventRow {
    pub(crate) seq: u64,
    pub(crate) at: u64,
    pub(crate) transport: Transport,
    pub(crate) acked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) feed_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) action_key: Option<String>,
    pub(crate) item: WaitItem,
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
        item: &WaitItem,
    ) {
        self.with(|log| {
            log.append_inbox(transport, feed_id, action_key, item)
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
        self.with(|log| log.maybe_compact());
    }
}

/// Handle on one (agent, source) log. The ingest side holds one of
/// these for the life of the wait; the ack CLI opens its own — both
/// funnel every write through the sidecar lock.
pub(crate) struct EventLog {
    log_path: PathBuf,
    state_path: PathBuf,
    lock_path: PathBuf,
    next_seq: u64,
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
            next_seq: 1,
            lines: 0,
        })
    }

    /// Parse the raw log text. `Err(())` = interior corruption. A
    /// torn final line (unparseable AND unterminated) is dropped as a
    /// normal crash artifact.
    fn parse(text: &str) -> Result<Vec<Record>, ()> {
        let mut out = Vec::new();
        let lines: Vec<&str> = text.split('\n').collect();
        let ends_with_newline = text.ends_with('\n') || text.is_empty();
        let n = lines.len();
        for (i, line) in lines.iter().enumerate() {
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Record>(line) {
                Ok(r) => out.push(r),
                // Only the physically-last, unterminated line may be
                // torn; anything else is real corruption.
                Err(_) if i == n - 1 && !ends_with_newline => {}
                Err(_) => return Err(()),
            }
        }
        Ok(out)
    }

    fn fold(records: Vec<Record>) -> (Vec<InboxEntry>, Vec<String>, Vec<String>, u64, usize) {
        let mut acked = std::collections::HashSet::new();
        for r in &records {
            if let Record::Ack { seq, .. } = r {
                acked.insert(*seq);
            }
        }
        let mut unhandled = Vec::new();
        let mut feed_ids = Vec::new();
        let mut action_keys = Vec::new();
        let mut max_seq = 0u64;
        let lines = records.len();
        for r in records {
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
        (unhandled, feed_ids, action_keys, max_seq, lines)
    }

    /// Inspector read for the `clank events` CLI: every inbox entry
    /// with its ack status, append order. READ-ONLY — unlike [`load`]
    /// it never quarantines (an inspector must not mutate the ingest
    /// side's artifacts); interior corruption is `Ok(None)` for the
    /// caller to report.
    pub(crate) fn read_rows(&self) -> std::io::Result<Option<Vec<EventRow>>> {
        let text = {
            let _lock = SidecarLock::acquire(&self.lock_path, false).ok();
            match std::fs::read_to_string(&self.log_path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Some(Vec::new()));
                }
                Err(e) => return Err(e),
            }
        };
        let Ok(records) = Self::parse(&text) else {
            return Ok(None);
        };
        let acked: std::collections::HashSet<u64> = records
            .iter()
            .filter_map(|r| match r {
                Record::Ack { seq, .. } => Some(*seq),
                _ => None,
            })
            .collect();
        let mut rows = Vec::new();
        for r in records {
            if let Record::Inbox {
                seq,
                at,
                transport,
                feed_id,
                action_key,
                item,
            } = r
            {
                rows.push(EventRow {
                    seq,
                    at,
                    transport,
                    acked: acked.contains(&seq),
                    feed_id,
                    action_key,
                    item,
                });
            }
        }
        rows.sort_by_key(|r| r.seq);
        Ok(Some(rows))
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
            Ok(records) => {
                let (unhandled, feed_ids, action_keys, max_seq, lines) = Self::fold(records);
                out.warm = true;
                out.unhandled = unhandled;
                out.feed_ids = feed_ids;
                out.action_keys = action_keys;
                self.next_seq = max_seq + 1;
                self.lines = lines;
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
        item: &WaitItem,
    ) -> std::io::Result<u64> {
        let seq = self.next_seq;
        self.append(&Record::Inbox {
            seq,
            at: now_epoch(),
            transport,
            feed_id: feed_id.map(str::to_owned),
            action_key: action_key.map(str::to_owned),
            item: item.clone(),
        })?;
        self.next_seq += 1;
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
            at: now_epoch(),
            feed_id: feed_id.map(str::to_owned),
            action_key: action_key.map(str::to_owned),
            baseline,
        })
    }

    pub(crate) fn append_ack(&mut self, seq: u64) -> std::io::Result<()> {
        self.append(&Record::Ack {
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
    /// [`LOG_IDENTITY_CAP`] identities and ALL unhandled inbox records
    /// (any age), and replace the file by rename. Appenders hold the
    /// lock shared and open after acquiring it, so nothing races the
    /// replacement.
    pub(crate) fn maybe_compact(&mut self) -> std::io::Result<()> {
        if self.lines < COMPACT_AT {
            return Ok(());
        }
        let _lock = SidecarLock::acquire(&self.lock_path, true)?;
        let text = match std::fs::read_to_string(&self.log_path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let Ok(records) = Self::parse(&text) else {
            // Corruption is load's problem; don't compound it here.
            return Ok(());
        };
        let acked: std::collections::HashSet<u64> = records
            .iter()
            .filter_map(|r| match r {
                Record::Ack { seq, .. } => Some(*seq),
                _ => None,
            })
            .collect();
        // Identity-bearing records, oldest→newest, folded to (feed_id,
        // action_key, at, baseline) — keep only the newest cap.
        let mut identities: Vec<(Option<String>, Option<String>, u64, bool)> = Vec::new();
        let mut unhandled: Vec<Record> = Vec::new();
        for r in records {
            match r {
                Record::Inbox {
                    seq,
                    at,
                    feed_id,
                    action_key,
                    ..
                } if acked.contains(&seq) => {
                    if feed_id.is_some() || action_key.is_some() {
                        identities.push((feed_id, action_key, at, false));
                    }
                }
                Record::Inbox { .. } => unhandled.push(r),
                Record::Obs {
                    feed_id,
                    action_key,
                    at,
                    baseline,
                } => {
                    if feed_id.is_some() || action_key.is_some() {
                        identities.push((feed_id, action_key, at, baseline));
                    }
                }
                Record::Ack { .. } => {}
            }
        }
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
        let tmp = self.log_path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, out.as_bytes())?;
        std::fs::rename(&tmp, &self.log_path)?;
        self.lines = kept_lines;
        Ok(())
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
            .append_inbox(Transport::Poll, Some("101"), Some("review#1"), &item(1))
            .unwrap();
        let s2 = log
            .append_inbox(Transport::Relay, None, Some("review#2"), &item(2))
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
            .append_inbox(Transport::Poll, Some("1"), None, &item(1))
            .unwrap();
        log.append_inbox(Transport::Poll, Some("2"), None, &item(2))
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
            .append_inbox(Transport::Poll, Some("1"), None, &item(1))
            .unwrap();
        let mut fresh = log_in(&dir);
        fresh.load();
        let s2 = fresh
            .append_inbox(Transport::Poll, Some("2"), None, &item(2))
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
        log.append_inbox(Transport::Poll, Some("1"), None, &item(1))
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
        log.append_inbox(Transport::Poll, Some("1"), None, &item(1))
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
        log.append_inbox(Transport::Poll, Some("1"), None, &item(1))
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
            .append_inbox(Transport::Poll, Some("keep-me"), Some("k#1"), &item(7))
            .unwrap();
        for i in 0..COMPACT_AT {
            let id = format!("{i}");
            let seq = log
                .append_inbox(Transport::Poll, Some(&id), None, &item(1))
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
        assert!(
            lines <= LOG_IDENTITY_CAP + 1,
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
                .append_inbox(Transport::Poll, Some(&id), None, &item(1))
                .unwrap();
            log.append_ack(seq).unwrap();
        }
        for i in 0..COMPACT_AT {
            let key = format!("relay#{i}");
            let seq = log
                .append_inbox(Transport::Relay, None, Some(&key), &item(1))
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
                .append_inbox(Transport::Poll, Some(&id), None, &item(1))
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
                    log.append_inbox(Transport::Relay, None, Some(&key), &item(i))
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
        log.append_inbox(Transport::Poll, Some("target"), Some("k#t"), &item(1))
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
