//! Passive native deadlines. The lifecycle owner selects one absolute sleep;
//! this module owns no task, producer, watcher or idle ingest lease.

use super::event_source::{Durability, EmissionSink, SourceOrigin};
use super::github::log::{EventLog, IngestLease, Transport};
use super::source_ids::{Known, Seen};
use clank_core::{
    agent_config::{TimerSource, WaitEventSource},
    wait::WaitItem,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;

pub(crate) fn wall_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn parse(source: &TimerSource) -> anyhow::Result<u64> {
    anyhow::ensure!(
        !source.name.trim().is_empty(),
        "timer name must not be empty"
    );
    let instant = time::OffsetDateTime::parse(
        &source.due_at,
        &time::format_description::well_known::Rfc3339,
    )
    .map_err(|e| {
        anyhow::anyhow!(
            "timer `{}` has invalid due_at `{}`: {e}",
            source.name,
            source.due_at
        )
    })?;
    // `time` accepts RFC3339 leap seconds by clamping them. V1 must not.
    let seconds = source
        .due_at
        .split_once(['T', 't'])
        .and_then(|(_, clock)| clock.get(6..8));
    anyhow::ensure!(
        seconds != Some("60"),
        "timer `{}` due_at must not use a leap second",
        source.name
    );
    anyhow::ensure!(
        instant.nanosecond() == 0,
        "timer `{}` due_at must use whole seconds",
        source.name
    );
    let due = u64::try_from(instant.unix_timestamp())
        .map_err(|_| anyhow::anyhow!("timer `{}` due_at precedes the Unix epoch", source.name))?;
    Ok(due)
}

struct Entry {
    source: TimerSource,
    key: String,
    due: u64,
    failure: Option<String>,
}

/// Ordered deadlines plus failed entries awaiting an *existing* owner wake.
/// Failed due entries leave the sleep set, preventing a zero-delay hot loop.
pub(crate) struct TimerSet {
    entries: BTreeMap<(Instant, String), Entry>,
    recovery: Vec<Entry>,
    dir: PathBuf,
    sink: EmissionSink,
    invocation_keys: BTreeSet<String>,
}

impl TimerSet {
    pub(crate) fn new(
        sources: &[(WaitEventSource, SourceOrigin)],
        dir: &Path,
        invocation: Option<&str>,
        sink: EmissionSink,
    ) -> anyhow::Result<Self> {
        let mut entries = BTreeMap::new();
        let mut names = HashSet::new();
        let mut keys = HashSet::new();
        let mut invocation_keys = BTreeSet::new();
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let now = Instant::now();
        for (source, origin) in sources {
            let WaitEventSource::Timer(source) = source else {
                continue;
            };
            let scope = match origin {
                SourceOrigin::Config => None,
                SourceOrigin::Cli => Some(invocation.ok_or_else(|| {
                    anyhow::anyhow!("invocation timer requires an invocation identity")
                })?),
            };
            let key = source.source_key(scope);
            anyhow::ensure!(
                names.insert((scope, source.name.clone())) && keys.insert(key.clone()),
                "duplicate timer name or storage key `{}` in one scope",
                source.name
            );
            let due = parse(source)?;
            let deadline = now
                .checked_add(Duration::from_secs(due).saturating_sub(wall))
                .ok_or_else(|| anyhow::anyhow!("timer deadline outside monotonic range"))?;
            if scope.is_some() {
                invocation_keys.insert(key.clone());
            }
            entries.insert(
                (deadline, key.clone()),
                Entry {
                    source: source.clone(),
                    key,
                    due,
                    failure: None,
                },
            );
        }
        Ok(Self {
            entries,
            recovery: Vec::new(),
            dir: dir.to_owned(),
            sink,
            invocation_keys,
        })
    }

    pub(crate) fn invocation_keys(&self) -> &BTreeSet<String> {
        &self.invocation_keys
    }
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.entries.first_key_value().map(|(key, _)| key.0)
    }

    pub(crate) fn due(&mut self) {
        if self
            .next_deadline()
            .is_none_or(|deadline| deadline > Instant::now())
        {
            return;
        }
        self.due_at(wall_now(), Instant::now());
    }

    fn due_at(&mut self, wall: u64, now: Instant) {
        let mut ready = Vec::new();
        while self
            .entries
            .first_key_value()
            .is_some_and(|(key, _)| key.0 <= now)
        {
            let (_, entry) = self.entries.pop_first().expect("checked nonempty");
            ready.push(entry);
        }
        ready.sort_by(|a, b| (a.due, &a.key).cmp(&(b.due, &b.key)));
        for entry in ready {
            if entry.due > wall {
                // Backward wall-clock movement: never ingest early.
                if let Some(deadline) = now.checked_add(Duration::from_secs(entry.due - wall)) {
                    self.entries.insert((deadline, entry.key.clone()), entry);
                } else {
                    self.recovery.push(entry);
                }
            } else if let Some(entry) = self.try_entry(entry) {
                self.recovery.push(entry);
            }
        }
    }

    /// Healthy future timers do not read clocks or files on heartbeats.
    pub(crate) fn retry(&mut self) {
        if self.recovery.is_empty() {
            return;
        }
        let wall = wall_now();
        for entry in std::mem::take(&mut self.recovery) {
            if entry.due > wall {
                if let Some(deadline) =
                    Instant::now().checked_add(Duration::from_secs(entry.due - wall))
                {
                    self.entries.insert((deadline, entry.key.clone()), entry);
                } else {
                    self.recovery.push(entry);
                }
                continue;
            }
            if let Some(entry) = self.try_entry(entry) {
                self.recovery.push(entry);
            }
        }
    }

    fn try_entry(&self, mut entry: Entry) -> Option<Entry> {
        match accept(&self.dir, &entry.key, &entry.source, entry.due, &self.sink) {
            Ok(()) => None,
            Err(error) => {
                let failure = error.to_string();
                if entry.failure.as_ref() != Some(&failure) {
                    eprintln!("wait: timer `{}`: {failure}", entry.source.name);
                    entry.failure = Some(failure);
                }
                Some(entry)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(name: &str, due: u64) -> TimerSource {
        TimerSource {
            name: name.into(),
            due_at: time::OffsetDateTime::from_unix_timestamp(due as i64)
                .unwrap()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
            content: Some("do the task".into()),
        }
    }
    fn set(
        dir: &Path,
        sources: Vec<TimerSource>,
    ) -> (
        TimerSet,
        tokio::sync::mpsc::UnboundedReceiver<super::super::event_source::Emission>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let specs: Vec<_> = sources
            .into_iter()
            .map(|s| (WaitEventSource::Timer(s), SourceOrigin::Config))
            .collect();
        (TimerSet::new(&specs, dir, None, tx.into()).unwrap(), rx)
    }
    fn rows(dir: &Path, key: &str) -> Vec<super::super::github::log::EventRow> {
        EventLog::open(dir, key)
            .unwrap()
            .read_rows()
            .unwrap()
            .unwrap()
            .rows
    }

    #[test]
    fn validates_and_normalizes_absolute_whole_seconds() {
        let mut timer = source("deploy", 1);
        for spelling in [
            "1970-01-01T00:00:01Z",
            "1970-01-01T11:00:01+11:00",
            "1970-01-01T00:00:01.000Z",
        ] {
            timer.due_at = spelling.into();
            assert_eq!(parse(&timer).unwrap(), 1);
        }
        for spelling in [
            "1970-01-01T00:00:01",
            "1970-01-01T00:00:01.001Z",
            "1969-12-31T23:59:59Z",
            "2016-12-31T23:59:60Z",
            "bad",
        ] {
            timer.due_at = spelling.into();
            assert!(parse(&timer).is_err(), "{spelling}");
        }
        timer = source("   ", 1);
        assert!(parse(&timer).is_err());
    }

    #[test]
    fn scope_and_lossless_name_hash_prevent_aliasing() {
        let a = source("a/b", 1);
        let b = source("a-b", 1);
        assert_ne!(a.source_key(None), b.source_key(None));
        assert_ne!(a.source_key(None), a.source_key(Some("one")));
        assert_ne!(a.source_key(Some("one")), a.source_key(Some("two")));
        assert!(a.source_key(None).starts_with("timer-config-"));
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let duplicate = [
            (WaitEventSource::Timer(a.clone()), SourceOrigin::Config),
            (WaitEventSource::Timer(a.clone()), SourceOrigin::Config),
        ];
        assert!(TimerSet::new(&duplicate, dir.path(), None, tx.clone().into()).is_err());
        let distinct = [
            (WaitEventSource::Timer(a.clone()), SourceOrigin::Config),
            (WaitEventSource::Timer(a), SourceOrigin::Cli),
        ];
        assert!(TimerSet::new(&distinct, dir.path(), Some("one"), tx.into()).is_ok());
    }

    #[tokio::test]
    async fn deadline_rechecks_wall_clock_and_traffic_does_not_reset_it() {
        let dir = tempfile::tempdir().unwrap();
        let due = wall_now() + 60;
        let (mut timers, mut rx) = set(dir.path(), vec![source("future", due)]);
        let deadline = timers.next_deadline().unwrap();
        for _ in 0..20 {
            timers.retry();
            timers.due();
            assert_eq!(timers.next_deadline(), Some(deadline));
        }
        assert!(rx.try_recv().is_err());
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "future timer holds no files/lease"
        );
        timers.due_at(due - 5, deadline);
        assert_eq!(
            timers.next_deadline(),
            Some(deadline + Duration::from_secs(5))
        );
        assert!(rx.try_recv().is_err());
        timers.due_at(due, deadline + Duration::from_secs(5));
        assert!(
            matches!(rx.recv().await.unwrap(), (WaitItem::TimerEvent { due_at, .. }, Durability::Logged) if due_at == due)
        );
        assert!(timers.next_deadline().is_none());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn late_arm_orders_occurrences_and_rearm_preserves_payload_and_ack() {
        let dir = tempfile::tempdir().unwrap();
        let a = source("z-older", 1);
        let b = source("a-newer", 2);
        let (mut timers, mut rx) = set(dir.path(), vec![b, a.clone()]);
        timers.due();
        assert!(matches!(
            rx.recv().await.unwrap().0,
            WaitItem::TimerEvent { due_at: 1, .. }
        ));
        assert!(matches!(
            rx.recv().await.unwrap().0,
            WaitItem::TimerEvent { due_at: 2, .. }
        ));
        let key = a.source_key(None);
        let original = rows(dir.path(), &key)[0].clone();
        assert_eq!(original.event_at, Some(1));
        assert!(original.at > 1);
        EventLog::open(dir.path(), &key)
            .unwrap()
            .append_ack_ident(original.seq, &original.ident)
            .unwrap();
        let mut edited = a;
        edited.content = Some("edited after acceptance".into());
        let (mut again, mut rx) = set(dir.path(), vec![edited]);
        again.due();
        assert!(rx.try_recv().is_err());
        let after = rows(dir.path(), &key);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].item, original.item);
        assert!(after[0].acked);
    }

    #[tokio::test]
    async fn lease_contention_disables_due_sleep_and_recovers_after_owner_dies() {
        let dir = tempfile::tempdir().unwrap();
        let src = source("locked", 1);
        let key = src.source_key(None);
        let lease = IngestLease::try_acquire(dir.path(), &key).unwrap().unwrap();
        let (mut timers, mut rx) = set(dir.path(), vec![src]);
        timers.due();
        assert!(
            timers.next_deadline().is_none(),
            "no zero-duration retry sleep"
        );
        assert_eq!(timers.recovery.len(), 1);
        assert!(rx.try_recv().is_err());
        timers.retry();
        assert_eq!(timers.recovery.len(), 1);
        drop(lease);
        timers.retry();
        assert!(timers.recovery.is_empty());
        assert!(matches!(
            rx.recv().await.unwrap().0,
            WaitItem::TimerEvent { .. }
        ));
        assert_eq!(rows(dir.path(), &key).len(), 1);
    }

    #[tokio::test]
    async fn failed_index_after_append_keeps_custody_and_repair_avoids_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let src = source("repair", 1);
        let key = src.source_key(None);
        Seen::at(dir.path(), &key).record("timer/1").unwrap();
        let entry = std::fs::read_dir(dir.path().join(format!("{key}.seen")))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::remove_file(&entry).unwrap();
        // The final identity is absent, but its atomic-write staging path
        // cannot be written. This fails *after* WAL append, not admission.
        let blocked = entry.with_extension("tmp");
        std::fs::create_dir(&blocked).unwrap();
        let (mut timers, mut rx) = set(dir.path(), vec![src]);
        timers.due();
        assert!(matches!(
            rx.recv().await.unwrap(),
            (WaitItem::TimerEvent { .. }, Durability::Logged)
        ));
        assert_eq!(rows(dir.path(), &key).len(), 1);
        assert_eq!(timers.recovery.len(), 1);
        std::fs::remove_dir(&blocked).unwrap();
        timers.retry();
        assert!(timers.recovery.is_empty());
        assert!(rx.try_recv().is_err());
        assert_eq!(rows(dir.path(), &key).len(), 1);
        assert_eq!(Seen::at(dir.path(), &key).known("timer/1"), Known::Yes);
    }

    #[tokio::test]
    async fn checkpoint_repairs_all_retained_ids_before_compaction_past_identity_horizon() {
        let dir = tempfile::tempdir().unwrap();
        let src = source("history", 1);
        let key = src.source_key(None);
        let mut log = EventLog::open(dir.path(), &key).unwrap();
        for due in 1..=2100 {
            let id = format!("timer/{due}");
            let item = WaitItem::TimerEvent {
                name: src.name.clone(),
                id: id.clone(),
                due_at: due,
                content: "old".into(),
                selector: None,
            };
            let seq = log
                .append_inbox(Transport::Timer, Some(&id), None, Some(due), &item)
                .unwrap();
            log.append_ack(seq).unwrap();
        }
        assert_eq!(log.retained_timer_ids().unwrap().len(), 2100);
        assert!(log.load().feed_ids.len() < 2100);
        // Failed durable checkpoint cannot compact the only retained proof.
        std::fs::write(dir.path().join(format!("{key}.seen")), "blocked").unwrap();
        let (mut timers, mut rx) = set(dir.path(), vec![src.clone()]);
        timers.due();
        assert_eq!(log.retained_timer_ids().unwrap().len(), 2100);
        assert!(rx.try_recv().is_err());
        std::fs::remove_file(dir.path().join(format!("{key}.seen"))).unwrap();
        timers.retry();
        assert!(timers.recovery.is_empty());
        assert!(
            log.retained_timer_ids().unwrap().len() < 2100,
            "checkpoint permits compaction"
        );
        assert_eq!(Seen::at(dir.path(), &key).known("timer/1"), Known::Yes);
        let (mut again, mut rx) = set(dir.path(), vec![src]);
        again.due();
        assert!(rx.try_recv().is_err());
        assert!(rows(dir.path(), &key).iter().all(|row| row.acked));
    }

    #[tokio::test]
    async fn corrupt_foreign_and_ambiguous_evidence_fail_closed() {
        for bad in ["not JSON\n", "{\"t\":\"future\",\"v\":2,\"seq\":42}\n"] {
            let dir = tempfile::tempdir().unwrap();
            let src = source("bad", 1);
            let key = src.source_key(None);
            let path = dir.path().join(format!("{key}.jsonl"));
            std::fs::write(&path, bad).unwrap();
            let (mut timers, mut rx) = set(dir.path(), vec![src]);
            timers.due();
            assert_eq!(std::fs::read_to_string(path).unwrap(), bad);
            assert!(rx.try_recv().is_err());
            assert_eq!(timers.recovery.len(), 1);
        }
        let dir = tempfile::tempdir().unwrap();
        let src = source("damaged-index", 1);
        let key = src.source_key(None);
        Seen::at(dir.path(), &key).record("timer/1").unwrap();
        let entry = std::fs::read_dir(dir.path().join(format!("{key}.seen")))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(entry, "wrong ID").unwrap();
        let (mut timers, mut rx) = set(dir.path(), vec![src]);
        timers.due();
        assert!(rx.try_recv().is_err());
        assert!(rows(dir.path(), &key).is_empty());
    }

    #[tokio::test]
    async fn discovery_survives_definition_removal_and_isolates_invocations() {
        let dir = tempfile::tempdir().unwrap();
        let src = source("backlog", 1);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for scope in [None, Some("one"), Some("two")] {
            accept(
                dir.path(),
                &src.source_key(scope),
                &src,
                1,
                &tx.clone().into(),
            )
            .unwrap();
            rx.recv().await.unwrap();
        }
        let mut backlog = Backlog::new(
            dir.path().to_owned(),
            BTreeSet::from([src.source_key(Some("one"))]),
        );
        let mut shown = HashSet::new();
        let got = backlog.owed(&mut shown);
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|item| {
            item.event_view()
                .unwrap()
                .selector
                .unwrap()
                .contains("timer-config-")
                || item
                    .event_view()
                    .unwrap()
                    .selector
                    .unwrap()
                    .contains("-one@")
        }));
        assert!(
            backlog.owed(&mut shown).is_empty(),
            "same client does not get two presentations"
        );
        let key = src.source_key(None);
        let row = rows(dir.path(), &key).remove(0);
        EventLog::open(dir.path(), &key)
            .unwrap()
            .append_ack_ident(row.seq, &row.ident)
            .unwrap();
        let mut restarted = Backlog::new(dir.path().to_owned(), BTreeSet::new());
        assert!(restarted.owed(&mut HashSet::new()).is_empty());
        assert_eq!(restarted.pending.as_ref().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn dropped_scheduler_cancels_future_and_closed_sink_keeps_accepted_work() {
        let dir = tempfile::tempdir().unwrap();
        let (timers, mut rx) = set(dir.path(), vec![source("future", wall_now() + 100)]);
        drop(timers);
        assert!(rx.recv().await.is_none());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        let src = source("accepted", 1);
        let (mut timers, rx) = set(dir.path(), vec![src.clone()]);
        drop(rx);
        timers.due();
        let mut backlog = Backlog::new(dir.path().to_owned(), BTreeSet::new());
        assert_eq!(backlog.owed(&mut HashSet::new()).len(), 1);
    }
}

/// Wait without manufacturing an idle sleep when there are no deadlines.
pub(crate) async fn deadline(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending::<()>().await,
    }
}

fn accept(
    dir: &Path,
    key: &str,
    source: &TimerSource,
    due: u64,
    sink: &EmissionSink,
) -> std::io::Result<()> {
    let Some(_lease) = IngestLease::try_acquire(dir, key)? else {
        return Err(std::io::Error::other(
            "ingest lease busy; retry on next owner wake",
        ));
    };
    let mut log = EventLog::open(dir, key)?;
    let ids = log.retained_timer_ids()?;
    // Strict inspection precedes load: timer corruption is preserved rather
    // than quarantined and reset as a replayable command source would be.
    log.load();
    let mut seen = Seen::at(dir, key);
    seen.checkpoint(ids)?;
    let id = format!("timer/{due}");
    match seen.known(&id) {
        Known::Yes => {
            log.maybe_compact()?;
            return Ok(());
        }
        Known::Damaged(why) => {
            return Err(std::io::Error::other(format!(
                "ambiguous timer identity; repair required: {why}"
            )));
        }
        Known::No => {}
    }
    let mut item = WaitItem::TimerEvent {
        name: source.name.clone(),
        id: id.clone(),
        due_at: due,
        content: source.content.clone().unwrap_or_default(),
        selector: None,
    };
    let seq = log.append_inbox(Transport::Timer, Some(&id), None, Some(due), &item)?;
    let checkpoint = seen.record(&id);
    super::command_source::stamp_selector(&mut item, key, seq);
    // A failed index write or closed sink cannot retract accepted WAL custody.
    let _ = sink.send((item, Durability::Logged));
    checkpoint?;
    log.maybe_compact()?;
    Ok(())
}

/// Disposable pending-log inventory. Discovery occurs on initial admission or
/// watcher invalidation; healthy empty inventory performs no heartbeat I/O.
pub(crate) struct Backlog {
    dir: PathBuf,
    invocation_keys: BTreeSet<String>,
    pending: Option<BTreeSet<String>>,
}

impl Backlog {
    pub(crate) fn new(dir: PathBuf, invocation_keys: BTreeSet<String>) -> Self {
        Self {
            dir,
            invocation_keys,
            pending: None,
        }
    }
    pub(crate) fn invalidate(&mut self) {
        self.pending = None;
    }

    pub(crate) fn owed(&mut self, shown: &mut HashSet<(String, u64, String)>) -> Vec<WaitItem> {
        if self.pending.is_none() {
            let mut keys = self.invocation_keys.clone();
            match std::fs::read_dir(&self.dir) {
                Ok(files) => {
                    for file in files {
                        let Ok(file) = file else { return Vec::new() };
                        let path = file.path();
                        if path.extension().is_some_and(|ext| ext == "jsonl")
                            && let Some(key) = path.file_stem().and_then(|name| name.to_str())
                            && key.starts_with("timer-config-")
                        {
                            keys.insert(key.to_owned());
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                // Do not cache a failed scan as an authoritative empty inbox.
                // Existing owner wakes can retry after the read error clears.
                Err(_) => return Vec::new(),
            }
            self.pending = Some(keys);
        }
        let mut items = Vec::new();
        self.pending.as_mut().expect("initialized").retain(|key| {
            if !self.dir.exists() {
                return false;
            }
            let Ok(log) = EventLog::open(&self.dir, key) else {
                return true;
            };
            let Ok(Some(read)) = log.read_rows() else {
                return true;
            };
            let rows: Vec<_> = read
                .rows
                .into_iter()
                .filter(|row| {
                    row.transport == Transport::Timer
                        && matches!(row.item, WaitItem::TimerEvent { .. })
                        && !row.acked
                })
                .collect();
            let pending = !rows.is_empty();
            items.extend(super::command_source::owed_from_log(rows, key, None, shown));
            pending
        });
        items
    }
}
