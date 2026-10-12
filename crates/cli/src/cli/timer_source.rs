//! Passive native deadlines. The lifecycle owner selects one absolute sleep;
//! this module owns no task, producer, watcher or idle ingest lease.

pub(crate) mod recurrence;

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
    if let Some(interval) = recurrence::interval(source)? {
        anyhow::ensure!(
            due.checked_add(interval).is_some(),
            "timer `{}` recurrence exceeds the timestamp range",
            source.name
        );
    }
    Ok(due)
}

/// What a configured timer's durable evidence says about it. Times are
/// absolute, so whether a due time has passed is the reader's clock to
/// judge, not this snapshot's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TimerState {
    /// Its next occurrence is due at this time and not yet accepted:
    /// in the future it is scheduled, in the past it is owed.
    Due(u64),
    /// A one-off whose occurrence is durably accepted.
    Fired,
    /// A recurring timer's occurrence awaits its ack; no next deadline is
    /// armed until it is acked.
    AwaitingAck,
    /// The evidence cannot be read, or contradicts itself.
    Unknown(String),
}

/// The state of `source`, configured in the events directory `dir`, from
/// the evidence the scheduler itself reads: the one-off's exact
/// occurrence id in its WAL or its `Seen` index, or the recurring WAL's
/// strict fold. Reads only: no lease, no lock, no file or directory made,
/// nothing repaired or compacted. A missing log is no evidence, which
/// means nothing accepted; an unreadable one is unknown.
pub(crate) fn inspect(dir: &Path, source: &TimerSource) -> TimerState {
    match inspected(dir, source) {
        Ok(state) => state,
        Err(why) => TimerState::Unknown(why.to_string()),
    }
}

fn inspected(dir: &Path, source: &TimerSource) -> anyhow::Result<TimerState> {
    let wal = |key: &str| -> anyhow::Result<String> {
        match std::fs::read_to_string(dir.join(format!("{key}.jsonl"))) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e.into()),
        }
    };
    if source.repeat_every.is_some() {
        let r = recurrence::Recurrence::parse(source, None)?;
        let (covered, pending) =
            super::github::log::recurring_progress(&wal(&r.source_key())?, &r)?;
        if pending {
            return Ok(TimerState::AwaitingAck);
        }
        return Ok(TimerState::Due(r.next_due(covered)?));
    }
    let due = parse(source)?;
    let key = source.source_key(None);
    let id = format!("timer/{due}");
    if EventLog::retained_timer_ids_in(&wal(&key)?)?.contains(&id) {
        return Ok(TimerState::Fired);
    }
    Ok(match Seen::at(dir, &key).known(&id) {
        Known::Yes => TimerState::Fired,
        Known::No => TimerState::Due(due),
        Known::Damaged(why) => TimerState::Unknown(why),
    })
}

/// The storage key the scheduler gives a configured timer: a recurring
/// timer's log is its generation's, not its name's.
pub(crate) fn config_key(source: &TimerSource) -> String {
    match source.repeat_every {
        Some(_) => recurrence::Recurrence::parse(source, None)
            .map_or_else(|_| source.source_key(None), |r| r.source_key()),
        None => source.source_key(None),
    }
}

struct Entry {
    source: TimerSource,
    key: String,
    due: u64,
    failure: Option<String>,
    recurrence: Option<recurrence::Recurrence>,
}

/// Ordered deadlines plus failed entries awaiting an *existing* owner wake.
/// Failed due entries leave the sleep set, preventing a zero-delay hot loop.
pub(crate) struct TimerSet {
    entries: BTreeMap<(Instant, String), Entry>,
    recovery: Vec<Entry>,
    pending: BTreeMap<String, Entry>,
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
            let recurrence = source
                .repeat_every
                .as_ref()
                .map(|_| recurrence::Recurrence::parse(source, scope))
                .transpose()?;
            let key = recurrence
                .as_ref()
                .map_or_else(|| source.source_key(scope), |r| r.source_key());
            anyhow::ensure!(
                names.insert((scope, source.name.clone())) && keys.insert(key.clone()),
                "duplicate timer name or storage key `{}` in one scope",
                source.name
            );
            let due = parse(source)?;
            if let Some(r) = &recurrence {
                anyhow::ensure!(
                    now.checked_add(Duration::from_secs(r.generation.interval))
                        .is_some(),
                    "timer `{}` recurrence interval outside monotonic range",
                    source.name
                );
            }
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
                    recurrence,
                },
            );
        }
        Ok(Self {
            entries,
            recovery: Vec::new(),
            pending: BTreeMap::new(),
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

    /// Constructors are passive, so a complete replacement can be validated
    /// before disturbing a running host. Arm once after taking ownership.
    pub(crate) fn arm(&mut self) {
        self.refresh();
        self.due();
    }

    /// Refresh only on admission or existing event-directory invalidation.
    /// Pending entries have no deadline and never enter heartbeat retries.
    pub(crate) fn refresh(&mut self) {
        self.refresh_at(wall_now(), Instant::now());
    }

    fn refresh_at(&mut self, wall: u64, now: Instant) {
        let keys: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.recurrence.is_some())
            .map(|(key, _)| key.clone())
            .collect();
        let mut refresh = Vec::new();
        for key in keys {
            refresh.push((
                self.entries.remove(&key).expect("collected key"),
                Some(key.0),
            ));
        }
        refresh.extend(
            std::mem::take(&mut self.pending)
                .into_values()
                .map(|entry| (entry, None)),
        );
        for entry in std::mem::take(&mut self.recovery) {
            if entry.recurrence.is_some() {
                refresh.push((entry, None));
            } else {
                self.recovery.push(entry);
            }
        }
        for (entry, deadline) in refresh {
            self.try_entry(entry, wall, now, deadline);
        }
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
            if entry.recurrence.is_none() && entry.due > wall {
                // Backward wall-clock movement: never ingest early.
                if let Some(deadline) = now.checked_add(Duration::from_secs(entry.due - wall)) {
                    self.entries.insert((deadline, entry.key.clone()), entry);
                } else {
                    self.recovery.push(entry);
                }
            } else {
                self.try_entry(entry, wall, now, None);
            }
        }
    }

    /// Healthy future timers do not read clocks or files on heartbeats.
    pub(crate) fn retry(&mut self) {
        self.retry_entries(true);
    }

    pub(crate) fn retry_one_shots(&mut self) {
        self.retry_entries(false);
    }

    fn retry_entries(&mut self, recurring: bool) {
        if self.recovery.is_empty() {
            return;
        }
        let wall = wall_now();
        for entry in std::mem::take(&mut self.recovery) {
            if !recurring && entry.recurrence.is_some() {
                self.recovery.push(entry);
                continue;
            }
            if entry.recurrence.is_none() && entry.due > wall {
                if let Some(deadline) =
                    Instant::now().checked_add(Duration::from_secs(entry.due - wall))
                {
                    self.entries.insert((deadline, entry.key.clone()), entry);
                } else {
                    self.recovery.push(entry);
                }
                continue;
            }
            self.try_entry(entry, wall, Instant::now(), None);
        }
    }

    fn try_entry(&mut self, mut entry: Entry, wall: u64, now: Instant, previous: Option<Instant>) {
        let result = match &entry.recurrence {
            None => {
                accept(&self.dir, &entry.key, &entry.source, entry.due, &self.sink).map(|_| None)
            }
            Some(r) => (|| {
                let mut log = EventLog::open(&self.dir, &entry.key)?;
                let transition = log.accept_recurring_with_interrupt(
                    r,
                    entry.source.content.as_deref().unwrap_or_default(),
                    wall,
                    super::github::log::Interrupt::new(
                        entry.source.interrupt,
                        entry.source.interrupt_message.as_deref(),
                    ),
                )?;
                let next = match transition {
                    super::github::log::RecurringTransition::Future(due) => Some(due),
                    super::github::log::RecurringTransition::Pending => None,
                    super::github::log::RecurringTransition::Accepted { seq, mut item } => {
                        // Selectors are projection metadata, never persisted payload.
                        let (id, due) = match &item {
                            WaitItem::TimerEvent { id, due_at, .. } => (id.clone(), *due_at),
                            _ => unreachable!(),
                        };
                        item.set_presentation(log.presentation_key(
                            seq,
                            Transport::Timer,
                            Some(&id),
                            None,
                            Some(due),
                            &item,
                        ));
                        super::command_source::stamp_selector(&mut item, &entry.key, seq);
                        let _ = self.sink.send((item, Durability::Logged));
                        None
                    }
                };
                log.maybe_compact()?;
                Ok(next)
            })(),
        };
        match result {
            Ok(Some(due)) => {
                let deadline = previous
                    .filter(|deadline| entry.due == due && *deadline > now)
                    .or_else(|| now.checked_add(Duration::from_secs(due.saturating_sub(wall))));
                entry.due = due;
                if let Some(deadline) = deadline {
                    entry.failure = None;
                    self.entries.insert((deadline, entry.key.clone()), entry);
                } else {
                    if entry.failure.as_deref()
                        != Some("recurring deadline outside monotonic range")
                    {
                        eprintln!(
                            "wait: timer `{}`: recurring deadline outside monotonic range",
                            entry.source.name
                        );
                    }
                    entry.failure = Some("recurring deadline outside monotonic range".into());
                    self.recovery.push(entry);
                }
            }
            Ok(None) if entry.recurrence.is_some() => {
                entry.failure = None;
                self.pending.insert(entry.key.clone(), entry);
            }
            Ok(None) => {}
            Err(error) => {
                let failure = error.to_string();
                if entry.failure.as_ref() != Some(&failure) {
                    eprintln!("wait: timer `{}`: {failure}", entry.source.name);
                    entry.failure = Some(failure);
                }
                self.recovery.push(entry);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(name: &str, due: u64) -> TimerSource {
        TimerSource {
            interrupt: false,
            interrupt_message: None,
            name: name.into(),
            repeat_every: None,
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

    fn recurring_source(name: &str) -> TimerSource {
        let mut source = source(name, 100);
        source.repeat_every = Some("10s".into());
        source
    }

    #[tokio::test]
    async fn due_occurrences_keep_their_policy_after_source_changes() {
        for recurring in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut source = if recurring {
                recurring_source("urgent")
            } else {
                source("urgent", 100)
            };
            source.interrupt = true;
            source.interrupt_message = Some("Standup".into());
            let key = if recurring {
                recurrence::Recurrence::parse(&source, None)
                    .unwrap()
                    .source_key()
            } else {
                source.source_key(None)
            };
            let (mut timers, mut rx) = set(dir.path(), vec![source.clone()]);
            timers.due_at(100, Instant::now());
            assert!(rx.try_recv().is_ok());
            assert!(rows(dir.path(), &key)[0].interrupt);
            assert_eq!(
                rows(dir.path(), &key)[0].interrupt_hint.as_deref(),
                Some("Standup")
            );
            drop(timers);
            source.interrupt = false;
            source.interrupt_message = None;
            let (mut timers, _) = set(dir.path(), vec![source]);
            timers.due_at(200, Instant::now());
            timers.refresh_at(200, Instant::now());
            let retained = rows(dir.path(), &key);
            assert_eq!(retained.len(), 1, "pending occurrence must not duplicate");
            assert!(
                retained[0].interrupt,
                "policy belongs to the accepted occurrence"
            );
            assert_eq!(retained[0].interrupt_hint.as_deref(), Some("Standup"));
        }
    }

    #[tokio::test]
    async fn recurring_owner_sleeps_while_pending_and_ack_refresh_preserves_phase() {
        let dir = tempfile::tempdir().unwrap();
        let source = recurring_source("deploy");
        let r = recurrence::Recurrence::parse(&source, None).unwrap();
        let key = r.source_key();
        let (mut timers, mut rx) = set(dir.path(), vec![source]);
        let now = Instant::now();
        timers.refresh_at(100, now);
        let (item, _) = rx.try_recv().unwrap();
        assert!(
            item.event_view()
                .unwrap()
                .selector
                .unwrap()
                .starts_with(&key)
        );
        let row = rows(dir.path(), &key).remove(0);
        assert!(matches!(
            &row.item,
            WaitItem::TimerEvent { selector: None, .. }
        ));
        assert_eq!(timers.pending.len(), 1);
        assert!(timers.next_deadline().is_none());
        let path = dir.path().join(format!("{key}.jsonl"));
        let original = std::fs::read(&path).unwrap();
        // A heartbeat would expose this poison if it read the healthy pending
        // WAL. Neither retry nor due may perform that read or arm a deadline.
        std::fs::write(&path, "poison\n").unwrap();
        for _ in 0..20 {
            timers.retry();
            timers.due();
        }
        assert!(timers.recovery.is_empty());
        assert!(timers.next_deadline().is_none());
        assert!(rx.try_recv().is_err());
        std::fs::write(&path, original).unwrap();
        EventLog::open(dir.path(), &key)
            .unwrap()
            .ack_recurring_at(row.seq, &row.ident, 140)
            .unwrap();
        // Represents the final admission refresh after attaching a watcher,
        // including an ack that preceded that attachment.
        timers.refresh_at(145, now);
        assert!(timers.pending.is_empty());
        assert_eq!(timers.next_deadline(), Some(now + Duration::from_secs(5)));
        timers.due_at(149, now + Duration::from_secs(5));
        assert!(rx.try_recv().is_err(), "backward wall: no early acceptance");
        timers.due_at(150, now + Duration::from_secs(6));
        let (item, _) = rx.try_recv().unwrap();
        let WaitItem::TimerEvent {
            recurrence: Some(metadata),
            ..
        } = item
        else {
            unreachable!()
        };
        assert_eq!(
            (metadata.first_slot, metadata.last_slot, metadata.count),
            (5, 5, 1)
        );
        assert!(timers.next_deadline().is_none());
        assert_eq!(rows(dir.path(), &key).len(), 2);
    }

    #[tokio::test]
    async fn recurring_unrepresentable_next_deadline_disables_sleep_and_preserves_failure() {
        let dir = tempfile::tempdir().unwrap();
        let source = recurring_source("range");
        let key = recurrence::Recurrence::parse(&source, None)
            .unwrap()
            .source_key();
        let (mut timers, mut rx) = set(dir.path(), vec![source]);
        timers.refresh_at(100, Instant::now());
        rx.try_recv().unwrap();
        let row = rows(dir.path(), &key).remove(0);
        EventLog::open(dir.path(), &key)
            .unwrap()
            .ack_recurring_at(row.seq, &row.ident, u64::MAX - 200)
            .unwrap();
        timers.refresh_at(100, Instant::now());
        assert!(timers.next_deadline().is_none());
        assert_eq!(timers.recovery.len(), 1);
        let failure = timers.recovery[0].failure.clone();
        assert!(failure.is_some());
        timers.retry();
        assert_eq!(timers.recovery.len(), 1);
        assert_eq!(timers.recovery[0].failure, failure);
        assert!(timers.next_deadline().is_none());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn recurring_future_is_stable_and_pending_takeover_does_not_emit_again() {
        let dir = tempfile::tempdir().unwrap();
        let source = recurring_source("phase");
        let key = recurrence::Recurrence::parse(&source, None)
            .unwrap()
            .source_key();
        let (mut timers, mut rx) = set(dir.path(), vec![source.clone()]);
        let now = Instant::now();
        timers.refresh_at(95, now);
        assert_eq!(timers.next_deadline(), Some(now + Duration::from_secs(5)));
        for n in 0..4 {
            timers.refresh_at(95 + n, now + Duration::from_secs(n));
            assert_eq!(timers.next_deadline(), Some(now + Duration::from_secs(5)));
        }
        assert!(rx.try_recv().is_err());
        timers.due_at(100, now + Duration::from_secs(5));
        rx.try_recv().unwrap();
        drop(timers);
        let (mut successor, mut successor_rx) = set(dir.path(), vec![source]);
        successor.refresh_at(1_000_000, now);
        assert!(successor.next_deadline().is_none());
        assert_eq!(successor.pending.len(), 1);
        assert!(successor_rx.try_recv().is_err());
        assert_eq!(rows(dir.path(), &key).len(), 1);
        let mut removed = Backlog::new(dir.path().to_owned(), BTreeSet::new());
        assert_eq!(removed.owed(&mut HashSet::new()).len(), 1);
    }

    #[tokio::test]
    async fn recurring_backlog_caches_without_io_and_peek_does_not_repair_tails() {
        let dir = tempfile::tempdir().unwrap();
        let source = recurring_source("cached");
        let r = recurrence::Recurrence::parse(&source, None).unwrap();
        let key = r.source_key();
        let (mut timers, mut rx) = set(dir.path(), vec![source]);
        timers.refresh_at(120, Instant::now());
        rx.try_recv().unwrap();
        let path = dir.path().join(format!("{key}.jsonl"));
        let mut original = std::fs::read(&path).unwrap();
        assert_eq!(original.pop(), Some(b'\n'));
        std::fs::write(&path, &original).unwrap();
        let mut inspection = Backlog::new(dir.path().to_owned(), BTreeSet::new()).inspection();
        assert_eq!(inspection.owed(&mut HashSet::new()).len(), 1);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            original,
            "peek does not sync/repair"
        );
        let mut recovering = Backlog::new(dir.path().to_owned(), BTreeSet::new());
        let mut shown = HashSet::new();
        assert_eq!(recovering.owed(&mut shown).len(), 1);
        assert!(std::fs::read(&path).unwrap().ends_with(b"\n"));
        std::fs::write(&path, "poison\n").unwrap();
        for _ in 0..20 {
            assert!(recovering.owed(&mut shown).is_empty());
        }
        assert!(
            recovering.failures.is_empty(),
            "healthy cache does not reread WAL on beats"
        );
        recovering.invalidate();
        assert!(recovering.owed(&mut HashSet::new()).is_empty());
        assert_eq!(recovering.failures.len(), 1);
    }

    #[tokio::test]
    async fn recurring_owner_runs_automatic_compaction_without_per_tick_seen_files() {
        let dir = tempfile::tempdir().unwrap();
        let source = recurring_source("compact");
        let r = recurrence::Recurrence::parse(&source, None).unwrap();
        let key = r.source_key();
        let completed = super::super::github::log::COMPACT_AT as u64 / 2;
        // Seed prior real-shaped completed transitions in one fixture write;
        // the owner must hit its production threshold on the next acceptance.
        let mut bytes = Vec::new();
        for slot in 0..completed {
            let id = r.producer_id(slot).unwrap();
            let due = r.due_at(slot).unwrap();
            let item = WaitItem::TimerEvent {
                presentation: None,
                name: source.name.clone(),
                id: id.clone(),
                due_at: due,
                content: "old".into(),
                selector: None,
                recurrence: Some(clank_core::agent_config::TimerRecurrence {
                    generation: r.generation.clone(),
                    generation_id: r.id.clone(),
                    first_slot: slot,
                    last_slot: slot,
                    count: 1,
                }),
            };
            let ident = super::super::github::log::logical_ident(
                Some(&id),
                None,
                Some(due),
                Transport::Timer,
                &item,
            );
            for record in [
                serde_json::json!({"t":"inbox","v":1,"seq":slot+1,"at":due,"event_at":due,"transport":"timer","feed_id":id,"item":item}),
                serde_json::json!({"t":"ack","v":1,"seq":slot+1,"at":due,"ident":ident}),
            ] {
                serde_json::to_writer(&mut bytes, &record).unwrap();
                bytes.push(b'\n');
            }
        }
        let path = dir.path().join(format!("{key}.jsonl"));
        std::fs::write(&path, bytes).unwrap();
        let (mut timers, mut rx) = set(dir.path(), vec![source]);
        timers.refresh_at(r.due_at(completed).unwrap(), Instant::now());
        let (item, _) = rx.try_recv().unwrap();
        assert!(
            item.event_view()
                .unwrap()
                .selector
                .unwrap()
                .ends_with(&format!("@{}", completed + 1))
        );
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            contents.lines().count(),
            2 + 2 * super::super::github::log::DISPLAY_CAP
        );
        assert_eq!(
            contents
                .lines()
                .filter(|line| line.contains("\"t\":\"timer_progress\""))
                .count(),
            1
        );
        assert!(!dir.path().join(format!("{key}.seen")).exists());
        assert!(timers.next_deadline().is_none());
        assert_eq!(
            inspect(dir.path(), &recurring_source("compact")),
            TimerState::AwaitingAck,
            "the compacted WAL still proves the occurrence it awaits"
        );
        let row = rows(dir.path(), &key)
            .into_iter()
            .find(|row| !row.acked)
            .unwrap();
        EventLog::open(dir.path(), &key)
            .unwrap()
            .ack_recurring_at(row.seq, &row.ident, r.due_at(completed).unwrap())
            .unwrap();
        assert_eq!(
            inspect(dir.path(), &recurring_source("compact")),
            TimerState::Due(r.due_at(completed + 1).unwrap()),
            "compacted coverage carries through to the next slot"
        );
    }

    /// Every path under `dir` with its bytes, to prove an inspection
    /// wrote nothing.
    fn tree(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(at) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&at) else {
                continue;
            };
            for entry in entries {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    out.insert(path.clone(), Vec::new());
                    stack.push(path);
                } else {
                    out.insert(path.clone(), std::fs::read(&path).unwrap());
                }
            }
        }
        out
    }

    /// Inspect, asserting it touched nothing.
    fn looked(dir: &Path, source: &TimerSource) -> TimerState {
        let before = tree(dir);
        let state = inspect(dir, source);
        assert_eq!(tree(dir), before, "inspection wrote nothing");
        state
    }

    /// A one-off is due, scheduled or owed, until its exact occurrence is
    /// durably accepted, and fired from then on whatever the clock says.
    /// No log at all, or an acceptance that failed, is not evidence.
    #[tokio::test]
    async fn a_one_off_is_owed_until_its_occurrence_is_accepted() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("events");
        let timer = source("rama", 100);
        assert_eq!(looked(&dir, &timer), TimerState::Due(100));
        assert!(!dir.exists(), "no events directory was made");

        std::fs::create_dir_all(&dir).unwrap();
        let key = timer.source_key(None);
        let held = IngestLease::try_acquire(&dir, &key).unwrap().unwrap();
        let (mut timers, _rx) = set(&dir, vec![timer.clone()]);
        timers.due_at(150, Instant::now());
        assert_eq!(
            looked(&dir, &timer),
            TimerState::Due(100),
            "acceptance failed"
        );
        drop(held);
        timers.retry();
        assert_eq!(looked(&dir, &timer), TimerState::Fired);

        let seen = dir.join(format!("{key}.seen"));
        let index = tree(&seen);
        std::fs::remove_dir_all(&seen).unwrap();
        assert_eq!(
            looked(&dir, &timer),
            TimerState::Fired,
            "the WAL proves it across the crash gap before its Seen write"
        );
        for (path, bytes) in &index {
            if !bytes.is_empty() {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, bytes).unwrap();
            }
        }

        std::fs::remove_file(dir.join(format!("{key}.jsonl"))).unwrap();
        assert_eq!(
            looked(&dir, &timer),
            TimerState::Fired,
            "the Seen tier proves it after the WAL is gone"
        );
        let entry = std::fs::read_dir(&seen)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(&entry, "something else").unwrap();
        assert!(
            matches!(looked(&dir, &timer), TimerState::Unknown(_)),
            "damaged Seen"
        );
    }

    /// A recurring timer owes a catch-up for uncovered past slots, awaits
    /// its ack once one is accepted, and is next due after the ack; a WAL
    /// that cannot be read is unknown.
    #[tokio::test]
    async fn a_recurring_timer_states_what_its_wal_proves() {
        let dir = tempfile::tempdir().unwrap();
        let timer = recurring_source("review");
        let r = recurrence::Recurrence::parse(&timer, None).unwrap();
        let key = r.source_key();
        assert_eq!(
            looked(dir.path(), &timer),
            TimerState::Due(100),
            "slot 0 owed"
        );
        let (mut timers, _rx) = set(dir.path(), vec![timer.clone()]);
        timers.due_at(125, Instant::now());
        assert_eq!(looked(dir.path(), &timer), TimerState::AwaitingAck);
        let row = rows(dir.path(), &key).remove(0);
        EventLog::open(dir.path(), &key)
            .unwrap()
            .ack_recurring_at(row.seq, &row.ident, 126)
            .unwrap();
        assert_eq!(looked(dir.path(), &timer), TimerState::Due(130));
        std::fs::write(dir.path().join(format!("{key}.jsonl")), "garbage\nmore\n").unwrap();
        assert!(matches!(looked(dir.path(), &timer), TimerState::Unknown(_)));
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
                presentation: None,
                name: src.name.clone(),
                recurrence: None,
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
        presentation: None,
        name: source.name.clone(),
        recurrence: None,
        id: id.clone(),
        due_at: due,
        content: source.content.clone().unwrap_or_default(),
        selector: None,
    };
    let seq = log.append_interruptible_inbox(
        Transport::Timer,
        Some(&id),
        None,
        Some(due),
        &item,
        super::github::log::Interrupt::new(source.interrupt, source.interrupt_message.as_deref()),
    )?;
    let checkpoint = seen.record(&id);
    item.set_presentation(log.presentation_key(
        seq,
        Transport::Timer,
        Some(&id),
        None,
        Some(due),
        &item,
    ));
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
    recurring: BTreeMap<String, Vec<super::github::log::EventRow>>,
    failures: BTreeMap<String, String>,
    recover: bool,
}

impl Backlog {
    pub(crate) fn new(dir: PathBuf, invocation_keys: BTreeSet<String>) -> Self {
        Self {
            dir,
            invocation_keys,
            pending: None,
            recurring: BTreeMap::new(),
            failures: BTreeMap::new(),
            recover: true,
        }
    }
    pub(crate) fn invalidate(&mut self) {
        self.pending = None;
        self.recurring.clear();
    }

    pub(crate) fn inspection(mut self) -> Self {
        self.recover = false;
        self
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
            if let Some(rows) = self.recurring.get(key) {
                let fresh = rows
                    .iter()
                    .filter(|row| !shown.contains(&(key.clone(), row.seq, row.ident.clone())))
                    .cloned()
                    .collect();
                items.extend(super::command_source::owed_from_log(
                    fresh, key, None, shown,
                ));
                return !rows.is_empty();
            }
            if !self.dir.exists() {
                return false;
            }
            let Ok(mut log) = EventLog::open(&self.dir, key) else {
                return true;
            };
            let read = match log.read_timer_backlog(self.recover) {
                Ok(Some(read)) => {
                    self.failures.remove(key);
                    read
                }
                result => {
                    let failure = match result {
                        Ok(None) => "corrupt timer custody; repair required".to_owned(),
                        Err(error) => error.to_string(),
                        Ok(Some(_)) => unreachable!(),
                    };
                    if self.failures.get(key) != Some(&failure) {
                        eprintln!("wait: timer `{key}`: {failure}");
                        self.failures.insert(key.clone(), failure);
                    }
                    return true;
                }
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
            if log.is_recurring_key() {
                self.recurring.insert(key.clone(), rows.clone());
            }
            items.extend(super::command_source::owed_from_log(rows, key, None, shown));
            pending
        });
        items
    }
}
