//! Recurrence transitions stay inside EventLog's sidecar lock and WAL format.
//! TimerSet owns deadlines; this module owns only durable custody.

use super::*;
use crate::cli::timer_source::recurrence::Recurrence;
use clank_core::agent_config::TimerRecurrence;
use std::collections::{BTreeMap, HashMap};

/// The field must exist, including when it is null. Treating a damaged missing
/// checkpoint field as None would reopen already compacted completed slots.
pub(super) fn deserialize_coverage<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<Option<u64>, D::Error> {
    serde::Deserialize::deserialize(de)
}

struct Group {
    seq: u64,
    ident: String,
    rows: Vec<Record>,
    metadata: TimerRecurrence,
    ack: Option<Record>,
}

struct State {
    covered: Option<u64>,
    pending: Option<(u64, String)>,
    high: u64,
    groups: BTreeMap<(u64, String), Group>,
}

fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}

fn recurrence_of(lines: &[Line]) -> std::io::Result<Recurrence> {
    for line in lines {
        if let Line::Known(record) = line {
            let (generation, id) = match record.as_ref() {
                Record::TimerProgress {
                    generation,
                    generation_id,
                    ..
                } => (generation, generation_id),
                Record::Inbox {
                    item:
                        WaitItem::TimerEvent {
                            recurrence: Some(metadata),
                            ..
                        },
                    ..
                } => (&metadata.generation, &metadata.generation_id),
                _ => continue,
            };
            let r =
                Recurrence::from_generation(generation.clone()).map_err(std::io::Error::other)?;
            if &r.id != id {
                return Err(invalid("recurring generation hash mismatch"));
            }
            return Ok(r);
        }
    }
    Err(invalid("recurring WAL has no generation evidence"))
}

/// Strict fold uses the same canonical logical identity and ack matcher as
/// generic readers. Every incompatible record is preserved and refused.
fn fold(lines: &[Line], r: &Recurrence) -> std::io::Result<State> {
    let mut state = State {
        covered: None,
        pending: None,
        high: 0,
        groups: BTreeMap::new(),
    };
    let mut progress = false;
    let mut acks: HashMap<u64, Vec<Record>> = HashMap::new();
    let mut ids = HashMap::new();
    let mut interrupts = Vec::new();
    for line in lines {
        let Line::Known(record) = line else {
            return Err(invalid("foreign recurring WAL record; repair required"));
        };
        if record.version() != CURRENT_V {
            return Err(invalid("unsupported recurring WAL version"));
        }
        match record.as_ref() {
            Record::Interrupt { seq, .. } => {
                state.high = state.high.max(*seq);
                interrupts.push(record.as_ref().clone());
            }
            Record::TimerProgress {
                seq,
                generation,
                generation_id,
                covered_through,
                ..
            } => {
                if progress || *seq == 0 || generation != &r.generation || generation_id != &r.id {
                    return Err(invalid("conflicting recurring progress evidence"));
                }
                if let Some(slot) = covered_through {
                    r.due_at(*slot).map_err(std::io::Error::other)?;
                }
                progress = true;
                state.covered = *covered_through;
                state.high = state.high.max(*seq);
            }
            Record::Inbox {
                seq,
                at,
                event_at,
                baseline,
                transport,
                feed_id,
                action_key,
                item,
                ..
            } => {
                let WaitItem::TimerEvent {
                    name,
                    id,
                    due_at,
                    recurrence: Some(metadata),
                    selector,
                    ..
                } = item
                else {
                    return Err(invalid("non-recurring payload in recurring WAL"));
                };
                let count = metadata
                    .last_slot
                    .checked_sub(metadata.first_slot)
                    .and_then(|n| n.checked_add(1));
                if metadata.generation != r.generation
                    || metadata.generation_id != r.id
                    || name != &r.generation.name
                    || *transport != Transport::Timer
                    || *baseline
                    || action_key.is_some()
                    || selector.is_some()
                    || *seq == 0
                    || count != Some(metadata.count)
                    || *at < *due_at
                    || r.due_at(metadata.last_slot)
                        .map_err(std::io::Error::other)?
                        != *due_at
                    || event_at != &Some(*due_at)
                    || r.producer_id(metadata.last_slot)
                        .map_err(std::io::Error::other)?
                        != *id
                    || feed_id.as_ref() != Some(id)
                {
                    return Err(invalid("invalid recurring occurrence evidence"));
                }
                let ident = logical_ident(
                    feed_id.as_deref(),
                    action_key.as_deref(),
                    *event_at,
                    *transport,
                    item,
                );
                if ids
                    .insert(id.clone(), *seq)
                    .is_some_and(|other| other != *seq)
                {
                    return Err(invalid(
                        "recurring producer ID accepted at multiple sequences",
                    ));
                }
                let group = state
                    .groups
                    .entry((*seq, ident.clone()))
                    .or_insert_with(|| Group {
                        seq: *seq,
                        ident,
                        rows: Vec::new(),
                        metadata: metadata.clone(),
                        ack: None,
                    });
                // One identity cannot hide a conflicting payload in another
                // physical copy. Identical copies remain one logical obligation.
                if let Some(first) = group.rows.first()
                    && serde_json::to_vec(first).map_err(std::io::Error::other)?
                        != serde_json::to_vec(record.as_ref()).map_err(std::io::Error::other)?
                {
                    return Err(invalid("conflicting recurring physical copies"));
                }
                group.rows.push(record.as_ref().clone());
                state.high = state.high.max(*seq);
            }
            Record::Ack {
                seq, reply: None, ..
            } => {
                acks.entry(*seq).or_default().push(record.as_ref().clone());
                state.high = state.high.max(*seq);
            }
            _ => {
                return Err(invalid(
                    "unexpected recurring WAL semantics; repair required",
                ));
            }
        }
    }
    let mut claimed = std::collections::HashSet::new();
    for group in state.groups.values_mut() {
        if state
            .covered
            .is_none_or(|covered| group.metadata.last_slot > covered)
        {
            let first = match state.covered {
                None => 0,
                Some(slot) => slot
                    .checked_add(1)
                    .ok_or_else(|| invalid("recurring index exhausted"))?,
            };
            if group.metadata.first_slot != first {
                return Err(invalid("recurring occurrence skips unproven coverage"));
            }
        }
        for (index, ack) in acks.get(&group.seq).into_iter().flatten().enumerate() {
            let Record::Ack { seq, ident, at, .. } = ack else {
                unreachable!()
            };
            if !ack_covers(*seq, ident.as_deref(), group.seq, &group.ident) {
                continue;
            }
            claimed.insert((*seq, index));
            if let Some(Record::Ack { at: first, .. }) = &group.ack {
                if first != at {
                    return Err(invalid("conflicting recurring acknowledgement timestamps"));
                }
            } else {
                group.ack = Some(ack.clone());
            }
        }
        if let Some(Record::Ack { at, .. }) = &group.ack {
            state.covered = Some(
                r.ack_coverage(state.covered, group.metadata.last_slot, *at)
                    .map_err(std::io::Error::other)?,
            );
        } else if state
            .pending
            .replace((group.seq, group.ident.clone()))
            .is_some()
        {
            return Err(invalid("multiple pending recurring occurrences"));
        }
    }
    if acks
        .iter()
        .any(|(seq, rows)| (0..rows.len()).any(|index| !claimed.contains(&(*seq, index))))
    {
        return Err(invalid("recurring acknowledgement has no matching inbox"));
    }
    if let Some(key) = &state.pending {
        let row = &state.groups[key];
        let first = match state.covered {
            None => 0,
            Some(slot) => slot
                .checked_add(1)
                .ok_or_else(|| invalid("recurring index exhausted"))?,
        };
        if row.metadata.first_slot != first {
            return Err(invalid(
                "pending recurring range conflicts with proven coverage",
            ));
        }
    }
    for record in interrupts {
        let Record::Interrupt { seq, ref ident, .. } = record else {
            unreachable!()
        };
        let group = state
            .groups
            .get_mut(&(seq, ident.clone()))
            .ok_or_else(|| invalid("recurring interrupt has no matching occurrence"))?;
        if !group.rows.iter().any(|row| {
            matches!(
                row,
                Record::Inbox {
                    interrupt: true,
                    ..
                }
            )
        }) {
            return Err(invalid("recurring interrupt was not requested"));
        }
        group.rows.push(record);
    }
    Ok(state)
}

impl EventLog {
    /// Recovery publication syncs accepted evidence, including removed schedules.
    /// Inspection stays read-only. Caller never generates an occurrence here.
    pub(crate) fn read_timer_backlog(
        &mut self,
        recover: bool,
    ) -> std::io::Result<Option<ReadRows>> {
        if !recover || !self.is_recurring_key() {
            return self.read_rows();
        }
        let _lock = SidecarLock::acquire(&self.lock_path, true)?;
        let lines = self.recurring_lines()?;
        if !lines.is_empty() {
            self.validate_recurring_inspection(&lines)?;
            self.sync_existing_log()?;
        }
        self.read_rows_unlocked()
    }

    /// Inspectors preserve bytes while refusing ambiguous recurrence semantics.
    pub(super) fn validate_recurring_inspection(&self, lines: &[Line]) -> std::io::Result<()> {
        let r = recurrence_of(lines)?;
        self.validate_recurring_key(&r)?;
        fold(lines, &r)?;
        Ok(())
    }

    pub(crate) fn is_recurring_key(&self) -> bool {
        self.log_path
            .file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.strip_prefix("timer-config-recurring-")
                    .or_else(|| name.strip_prefix("timer-invocation-recurring-"))
                    .is_some_and(|id| id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
            })
    }

    fn recurring_lines(&self) -> std::io::Result<Vec<Line>> {
        let text = match std::fs::read_to_string(&self.log_path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        Self::parse(&text).map_err(|_| invalid("corrupt recurring WAL; repair required"))
    }

    fn validate_recurring_key(&self, r: &Recurrence) -> std::io::Result<()> {
        if self.log_path.file_stem().and_then(|name| name.to_str()) != Some(r.source_key().as_str())
        {
            return Err(invalid("recurring generation does not match WAL key"));
        }
        Ok(())
    }

    /// Revalidate and append under the ack/compaction write lock. A complete
    /// unsynced prior append is synced before it can be published on retry.
    pub(crate) fn accept_recurring(
        &mut self,
        r: &Recurrence,
        content: &str,
        wall: u64,
    ) -> std::io::Result<RecurringTransition> {
        self.accept_recurring_with_interrupt(r, content, wall, false)
    }

    pub(crate) fn accept_recurring_with_interrupt(
        &mut self,
        r: &Recurrence,
        content: &str,
        wall: u64,
        interrupt: bool,
    ) -> std::io::Result<RecurringTransition> {
        self.validate_recurring_key(r)?;
        let dir = self
            .log_path
            .parent()
            .ok_or_else(|| invalid("recurring WAL has no parent"))?;
        let Some(_lease) = IngestLease::try_acquire(dir, &r.source_key())? else {
            return Err(invalid(
                "recurring ingest lease busy; retry on next owner wake",
            ));
        };
        let _lock = SidecarLock::acquire(&self.lock_path, true)?;
        let lines = self.recurring_lines()?;
        let state = fold(&lines, r)?;
        self.lines = lines.len();
        self.next_seq = state.high.checked_add(1);
        if self.log_path.exists() {
            self.sync_existing_log()?;
        }
        if state.pending.is_some() {
            return Ok(RecurringTransition::Pending);
        }
        let Some(slots) = r
            .catch_up(state.covered, wall)
            .map_err(std::io::Error::other)?
        else {
            return Ok(RecurringTransition::Future(
                r.next_due(state.covered).map_err(std::io::Error::other)?,
            ));
        };
        let seq = self
            .next_seq
            .ok_or_else(|| invalid("recurring sequence exhausted"))?;
        let id = r.producer_id(slots.last).map_err(std::io::Error::other)?;
        let item = WaitItem::TimerEvent {
            name: r.generation.name.clone(),
            id: id.clone(),
            due_at: slots.due_at,
            content: content.into(),
            selector: None,
            recurrence: Some(TimerRecurrence {
                generation: r.generation.clone(),
                generation_id: r.id.clone(),
                first_slot: slots.first,
                last_slot: slots.last,
                count: slots.count,
            }),
        };
        self.append_durable_unlocked(&Record::Inbox {
            v: CURRENT_V,
            seq,
            at: wall,
            event_at: Some(slots.due_at),
            baseline: false,
            interrupt,
            transport: Transport::Timer,
            feed_id: Some(id),
            action_key: None,
            item: item.clone(),
        })?;
        self.next_seq = seq.checked_add(1);
        Ok(RecurringTransition::Accepted { seq, item })
    }

    /// Called ONLY while append_checked_ack holds the exclusive data lock.
    pub(super) fn append_recurring_ack_unlocked(
        &mut self,
        seq: u64,
        ident: &str,
        at: u64,
    ) -> std::io::Result<()> {
        let lines = self.recurring_lines()?;
        let r = recurrence_of(&lines)?;
        self.validate_recurring_key(&r)?;
        let state = fold(&lines, &r)?;
        let group = state
            .groups
            .get(&(seq, ident.into()))
            .ok_or_else(|| invalid("recurring ack target disappeared"))?;
        if group.ack.is_some() {
            return self.sync_existing_log();
        }
        // Validate the boundary before writing; backwards wall movement still
        // covers the accepted slot. The fold reconstructs this same decision.
        r.ack_coverage(state.covered, group.metadata.last_slot, at)
            .map_err(std::io::Error::other)?;
        self.lines = lines.len();
        self.append_durable_unlocked(&Record::Ack {
            v: CURRENT_V,
            seq,
            at,
            ident: Some(ident.into()),
            reply: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn ack_recurring_at(
        &mut self,
        seq: u64,
        ident: &str,
        wall: u64,
    ) -> std::io::Result<()> {
        let _lock = SidecarLock::acquire(&self.lock_path, true)?;
        self.append_recurring_ack_unlocked(seq, ident, wall)
    }

    /// Atomic rewrite of derived coverage together with all pending custody.
    pub(super) fn compact_recurring_unlocked(&mut self) -> std::io::Result<usize> {
        let lines = self.recurring_lines()?;
        if lines.is_empty() {
            return Ok(0);
        }
        let r = recurrence_of(&lines)?;
        self.validate_recurring_key(&r)?;
        let state = fold(&lines, &r)?;
        let mut records = vec![Record::TimerProgress {
            v: CURRENT_V,
            seq: state.high,
            generation: r.generation,
            generation_id: r.id,
            covered_through: state.covered,
        }];
        let handled = state
            .groups
            .values()
            .filter(|group| group.ack.is_some())
            .count();
        let mut discard = handled.saturating_sub(DISPLAY_CAP);
        for group in state.groups.into_values() {
            if group.ack.is_some() && discard > 0 {
                discard -= 1;
                continue;
            }
            records.extend(group.rows);
            if let Some(ack) = group.ack {
                records.push(ack);
            }
        }
        let mut encoded = Vec::new();
        for record in &records {
            serde_json::to_writer(&mut encoded, record).map_err(std::io::Error::other)?;
            encoded.push(b'\n');
        }
        self.rewrite_compacted_unlocked(&encoded, records.len())?;
        self.next_seq = state.high.checked_add(1);
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clank_core::agent_config::TimerGeneration;

    fn fixture() -> (tempfile::TempDir, Recurrence, EventLog) {
        let dir = tempfile::tempdir().unwrap();
        let r = Recurrence::from_generation(TimerGeneration {
            name: "deploy".into(),
            scope: None,
            anchor: 100,
            interval: 10,
        })
        .unwrap();
        let log = EventLog::open(dir.path(), &r.source_key()).unwrap();
        (dir, r, log)
    }

    #[test]
    fn recurring_interrupt_claim_survives_strict_recovery_and_compaction() {
        let (dir, r, mut log) = fixture();
        let (seq, ident, _) = accepted(&mut log, &r, 120);
        // Seed the new policy on this isolated occurrence; source API policy
        // propagation is a separate milestone, not implied by this fixture.
        let text = std::fs::read_to_string(&log.log_path).unwrap();
        let mut encoded = String::new();
        for line in text.lines() {
            let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
            if value["t"] == "inbox" {
                value["interrupt"] = true.into();
            }
            encoded.push_str(&serde_json::to_string(&value).unwrap());
            encoded.push('\n');
        }
        std::fs::write(&log.log_path, encoded).unwrap();
        assert!(log.claim_interrupt(seq, &ident).unwrap());
        log.lines = COMPACT_AT;
        log.maybe_compact().unwrap();
        let mut reopened = EventLog::open(dir.path(), &r.source_key()).unwrap();
        assert!(!reopened.claim_interrupt(seq, &ident).unwrap());
        let row = reopened
            .read_timer_backlog(true)
            .unwrap()
            .unwrap()
            .rows
            .remove(0);
        assert!(!row.acked);
        assert!(row.interrupt_attempt.is_some());
    }

    fn accepted(log: &mut EventLog, r: &Recurrence, wall: u64) -> (u64, String, WaitItem) {
        let RecurringTransition::Accepted { seq, item } =
            log.accept_recurring(r, "body\n🦀", wall).unwrap()
        else {
            panic!("not accepted")
        };
        let ident = logical_ident(
            match &item {
                WaitItem::TimerEvent { id, .. } => Some(id.as_str()),
                _ => unreachable!(),
            },
            None,
            item.event_view().unwrap().at,
            Transport::Timer,
            &item,
        );
        (seq, ident, item)
    }

    fn ack(log: &mut EventLog, seq: u64, ident: &str, wall: u64) {
        log.ack_recurring_at(seq, ident, wall).unwrap();
    }

    fn force_compact(log: &mut EventLog) {
        log.lines = COMPACT_AT;
        log.maybe_compact().unwrap();
    }

    #[test]
    fn recurring_sync_failures_refuse_publication_and_ack_retry_preserves_boundary() {
        for boundary in [
            super::super::DurableSync::File,
            super::super::DurableSync::Directory,
        ] {
            let (_dir, r, mut log) = fixture();
            log.sync_fault.set(Some(boundary));
            assert!(log.accept_recurring(&r, "original", 140).is_err());
            let visible = std::fs::read(&log.log_path).unwrap();
            assert!(
                log.read_timer_backlog(true).is_err(),
                "recovery must sync before delivery"
            );
            assert!(log.accept_recurring(&r, "replacement", 200).is_err());
            assert_eq!(std::fs::read(&log.log_path).unwrap(), visible);
            log.sync_fault.set(None);
            assert!(matches!(
                log.accept_recurring(&r, "replacement", 200).unwrap(),
                RecurringTransition::Pending
            ));
            let row = log
                .read_timer_backlog(true)
                .unwrap()
                .unwrap()
                .rows
                .remove(0);
            assert_eq!(row.item.event_view().unwrap().content, Some("original"));
            log.sync_fault.set(Some(boundary));
            assert!(log.ack_recurring_at(row.seq, &row.ident, 200).is_err());
            let ack_bytes = std::fs::read(&log.log_path).unwrap();
            assert!(log.ack_recurring_at(row.seq, &row.ident, 300).is_err());
            assert_eq!(std::fs::read(&log.log_path).unwrap(), ack_bytes);
            log.sync_fault.set(None);
            log.ack_recurring_at(row.seq, &row.ident, 300).unwrap();
            assert_eq!(std::fs::read(&log.log_path).unwrap(), ack_bytes);
            assert!(matches!(
                log.accept_recurring(&r, "next", 209).unwrap(),
                RecurringTransition::Future(210)
            ));
            let (_, _, next) = accepted(&mut log, &r, 210);
            assert!(
                matches!(next, WaitItem::TimerEvent { recurrence: Some(metadata), .. } if metadata.first_slot == 11 && metadata.count == 1)
            );
        }
    }

    #[test]
    fn recurring_compaction_sync_failures_keep_proven_coverage_and_pending_custody() {
        for boundary in [
            super::super::DurableSync::File,
            super::super::DurableSync::Directory,
        ] {
            let (dir, r, mut log) = fixture();
            let (seq, ident, _) = accepted(&mut log, &r, 140);
            ack(&mut log, seq, &ident, 150);
            let (_, _, pending) = accepted(&mut log, &r, 170);
            log.lines = COMPACT_AT;
            log.sync_fault.set(Some(boundary));
            assert!(log.maybe_compact().is_err());
            assert!(log.read_timer_backlog(true).is_err());
            let mut restarted = EventLog::open(dir.path(), &r.source_key()).unwrap();
            let rows = restarted.read_timer_backlog(true).unwrap().unwrap().rows;
            assert!(rows.iter().any(|row| row.item == pending && !row.acked));
            assert!(matches!(
                restarted.accept_recurring(&r, "new", 900).unwrap(),
                RecurringTransition::Pending
            ));
            force_compact(&mut restarted);
            let rows = restarted.read_rows().unwrap().unwrap().rows;
            let row = rows.iter().find(|row| row.item == pending).unwrap();
            ack(&mut restarted, row.seq, &row.ident, 900);
            assert!(matches!(
                restarted.accept_recurring(&r, "new", 909).unwrap(),
                RecurringTransition::Future(910)
            ));
        }
    }

    #[test]
    fn offline_catchup_pending_coalescing_ack_boundary_and_restart() {
        let (dir, r, mut log) = fixture();
        assert!(matches!(
            log.accept_recurring(&r, "", 99).unwrap(),
            RecurringTransition::Future(100)
        ));
        assert!(!log.log_path.exists(), "not due: no custody append");
        let (seq, ident, item) = accepted(&mut log, &r, 2_000_000_107);
        let WaitItem::TimerEvent {
            recurrence: Some(metadata),
            ..
        } = &item
        else {
            unreachable!()
        };
        assert_eq!(
            (metadata.first_slot, metadata.last_slot, metadata.count),
            (0, 200_000_000, 200_000_001)
        );
        let wire = serde_json::to_value(crate::cli::wait::render_json(&item)).unwrap();
        assert_eq!(wire["recurrence"]["count"], 200_000_001);
        assert_eq!(wire["recurrence"]["generation_id"], r.id);
        let before = std::fs::read(&log.log_path).unwrap();
        for wall in [2_000_000_117, 2_000_000_137, 3_000_000_000] {
            let mut restarted = EventLog::open(dir.path(), &r.source_key()).unwrap();
            assert!(matches!(
                restarted.accept_recurring(&r, "edited", wall).unwrap(),
                RecurringTransition::Pending
            ));
            assert_eq!(std::fs::read(&log.log_path).unwrap(), before);
        }
        ack(&mut log, seq, &ident, 2_000_000_140);
        ack(&mut log, seq, &ident, 3_000_000_000); // retry must not cover newer slots
        force_compact(&mut log);
        let mut restarted = EventLog::open(dir.path(), &r.source_key()).unwrap();
        assert!(matches!(
            restarted.accept_recurring(&r, "", 2_000_000_149).unwrap(),
            RecurringTransition::Future(2_000_000_150)
        ));
        let (_, _, next) = accepted(&mut restarted, &r, 2_000_000_200);
        let WaitItem::TimerEvent {
            recurrence: Some(next),
            ..
        } = next
        else {
            unreachable!()
        };
        assert_eq!(
            (next.first_slot, next.last_slot, next.count),
            (200_000_005, 200_000_010, 6)
        );
        assert_eq!(log.read_rows().unwrap().unwrap().rows.len(), 2);
    }

    #[test]
    fn compaction_bounds_tick_history_and_preserves_pending_and_allocator() {
        let (_dir, r, mut log) = fixture();
        // Cross the display retention window with real synced transitions,
        // forcing compaction without needing COMPACT_AT ticks in this test.
        for slot in 0..(DISPLAY_CAP as u64 + 20) {
            let (seq, ident, _) = accepted(&mut log, &r, 100 + slot * 10);
            ack(&mut log, seq, &ident, 100 + slot * 10);
        }
        force_compact(&mut log);
        let lines = log.recurring_lines().unwrap();
        assert_eq!(lines.len(), 1 + 2 * DISPLAY_CAP);
        assert_eq!(lines.iter().filter(|line| matches!(line, Line::Known(record) if matches!(record.as_ref(), Record::TimerProgress { .. }))).count(), 1);
        let (seq, _, original) = accepted(&mut log, &r, 999_999);
        assert_eq!(seq, DISPLAY_CAP as u64 + 21);
        force_compact(&mut log);
        assert!(matches!(
            log.accept_recurring(&r, "new", 9_999_999).unwrap(),
            RecurringTransition::Pending
        ));
        assert_eq!(
            log.read_rows().unwrap().unwrap().rows.last().unwrap().item,
            original
        );
        assert!(!log.log_path.with_extension("seen").exists());
    }

    #[test]
    fn backward_ack_clock_and_real_cli_ack_preserve_accepted_coverage() {
        let (_dir, r, mut log) = fixture();
        let (seq, ident, _) = accepted(&mut log, &r, 150);
        ack(&mut log, seq, &ident, 90);
        force_compact(&mut log);
        assert!(matches!(
            log.accept_recurring(&r, "", 140).unwrap(),
            RecurringTransition::Future(160)
        ));
        let (seq, ident, _) = accepted(&mut log, &r, 160);
        log.append_checked_ack(seq, &ident).unwrap();
        let before = std::fs::read(&log.log_path).unwrap();
        log.append_checked_ack(seq, &ident).unwrap();
        assert_eq!(std::fs::read(&log.log_path).unwrap(), before);
    }

    #[test]
    fn torn_append_repair_and_failed_compaction_keep_custody() {
        let (dir, r, mut log) = fixture();
        let (seq, ident, original) = accepted(&mut log, &r, 100);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&log.log_path)
            .unwrap();
        file.write_all(b"{\"t\":\"ack\",\"seq\":").unwrap();
        let mut restarted = EventLog::open(dir.path(), &r.source_key()).unwrap();
        assert!(matches!(
            restarted.accept_recurring(&r, "", 150).unwrap(),
            RecurringTransition::Pending
        ));
        assert!(std::fs::read(&log.log_path).unwrap().ends_with(b"\n"));
        assert_eq!(
            restarted.read_rows().unwrap().unwrap().rows[0].item,
            original
        );
        ack(&mut restarted, seq, &ident, 150);
        let before = std::fs::read(&log.log_path).unwrap();
        std::fs::create_dir(log.log_path.with_extension("jsonl.tmp")).unwrap();
        restarted.lines = COMPACT_AT;
        assert!(restarted.maybe_compact().is_err());
        assert_eq!(std::fs::read(&log.log_path).unwrap(), before);
        assert!(matches!(
            restarted.accept_recurring(&r, "", 159).unwrap(),
            RecurringTransition::Future(160)
        ));
    }

    #[test]
    fn refuses_foreign_corrupt_wrong_generation_and_conflicting_ack_without_reset() {
        for damage in ["garbage\n", "{\"t\":\"future\",\"v\":2,\"seq\":12}\n"] {
            let (_dir, r, mut log) = fixture();
            std::fs::write(&log.log_path, damage).unwrap();
            assert!(log.accept_recurring(&r, "", 100).is_err());
            assert_eq!(std::fs::read_to_string(&log.log_path).unwrap(), damage);
            assert!(!log.log_path.with_extension("jsonl.corrupt").exists());
        }
        let (_dir, r, mut log) = fixture();
        let mut other = r.generation.clone();
        other.interval = 20;
        assert!(
            log.accept_recurring(&Recurrence::from_generation(other).unwrap(), "", 100)
                .is_err()
        );
        let (seq, ident, _) = accepted(&mut log, &r, 100);
        ack(&mut log, seq, &ident, 110);
        log.append(&Record::Ack {
            v: CURRENT_V,
            seq,
            at: 120,
            ident: Some(ident),
            reply: None,
        })
        .unwrap();
        let before = std::fs::read(&log.log_path).unwrap();
        assert!(log.accept_recurring(&r, "", 200).is_err());
        log.lines = COMPACT_AT;
        assert!(log.maybe_compact().is_err());
        assert_eq!(std::fs::read(&log.log_path).unwrap(), before);
    }

    #[test]
    fn physical_copies_group_once_and_conflicting_payloads_cannot_hide() {
        let (_dir, r, mut log) = fixture();
        let (seq, ident, _) = accepted(&mut log, &r, 100);
        let Line::Known(row) = log.recurring_lines().unwrap().remove(0) else {
            unreachable!()
        };
        log.append(row.as_ref()).unwrap();
        assert!(matches!(
            log.accept_recurring(&r, "", 200).unwrap(),
            RecurringTransition::Pending
        ));
        ack(&mut log, seq, &ident, 200);
        force_compact(&mut log);
        assert!(matches!(
            log.accept_recurring(&r, "", 209).unwrap(),
            RecurringTransition::Future(210)
        ));
        let mut conflicting = *row;
        if let Record::Inbox {
            item: WaitItem::TimerEvent { content, .. },
            ..
        } = &mut conflicting
        {
            *content = "conflicting".into();
        }
        log.append(&conflicting).unwrap();
        assert!(log.accept_recurring(&r, "", 210).is_err());
    }

    #[test]
    fn ingest_contention_does_not_block_acknowledgement_or_duplicate_on_retry() {
        let (dir, r, mut log) = fixture();
        let (seq, ident, _) = accepted(&mut log, &r, 100);
        let lease = IngestLease::try_acquire(dir.path(), &r.source_key())
            .unwrap()
            .unwrap();
        let before = std::fs::read(&log.log_path).unwrap();
        assert!(log.accept_recurring(&r, "", 120).is_err());
        assert_eq!(std::fs::read(&log.log_path).unwrap(), before);
        ack(&mut log, seq, &ident, 110);
        drop(lease);
        assert!(matches!(
            log.accept_recurring(&r, "", 119).unwrap(),
            RecurringTransition::Future(120)
        ));
        accepted(&mut log, &r, 120);
        assert_eq!(
            log.read_rows()
                .unwrap()
                .unwrap()
                .rows
                .iter()
                .filter(|row| !row.acked)
                .count(),
            1
        );
    }

    #[test]
    fn concurrent_acceptance_and_ack_deadline_race_share_one_transition() {
        let (dir, r, mut log) = fixture();
        let (seq, ident, _) = accepted(&mut log, &r, 100);
        let gate = std::sync::Arc::new(std::sync::Barrier::new(3));
        std::thread::scope(|scope| {
            let accept_gate = gate.clone();
            let r = &r;
            let path = dir.path();
            let accepting = scope.spawn(move || {
                let mut writer = EventLog::open(path, &r.source_key()).unwrap();
                accept_gate.wait();
                writer.accept_recurring(r, "next", 130)
            });
            let ack_gate = gate.clone();
            let acknowledging = scope.spawn(move || {
                let mut writer = EventLog::open(path, &r.source_key()).unwrap();
                ack_gate.wait();
                ack(&mut writer, seq, &ident, 120);
            });
            gate.wait();
            let result = accepting.join().unwrap().unwrap();
            acknowledging.join().unwrap();
            match result {
                RecurringTransition::Pending => {
                    accepted(&mut log, r, 130);
                }
                RecurringTransition::Accepted {
                    item:
                        WaitItem::TimerEvent {
                            recurrence: Some(metadata),
                            ..
                        },
                    ..
                } => {
                    assert_eq!((metadata.first_slot, metadata.last_slot), (3, 3));
                }
                other => panic!("unexpected race transition: {other:?}"),
            }
        });
        let rows = log.read_rows().unwrap().unwrap().rows;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.iter().filter(|row| !row.acked).count(), 1);

        let next_gate = std::sync::Arc::new(std::sync::Barrier::new(3));
        std::thread::scope(|scope| {
            let mut jobs = Vec::new();
            for _ in 0..2 {
                let r = &r;
                let path = dir.path();
                let gate = next_gate.clone();
                jobs.push(scope.spawn(move || {
                    let mut writer = EventLog::open(path, &r.source_key()).unwrap();
                    gate.wait();
                    writer.accept_recurring(r, "", 200)
                }));
            }
            next_gate.wait();
            for job in jobs {
                assert!(matches!(
                    job.join().unwrap(),
                    Ok(RecurringTransition::Pending) | Err(_)
                ));
            }
        });
        assert_eq!(log.read_rows().unwrap().unwrap().rows.len(), 2);
    }

    #[test]
    fn complete_unsynced_tail_recovers_once_and_progress_alone_reserves_sequences() {
        let (dir, r, mut log) = fixture();
        let (seq, ident, _) = accepted(&mut log, &r, 100);
        // Simulate a complete visible append before newline/sync. Recovery
        // must finish framing and durability without accepting a second row.
        let mut bytes = std::fs::read(&log.log_path).unwrap();
        assert_eq!(bytes.pop(), Some(b'\n'));
        std::fs::write(&log.log_path, bytes).unwrap();
        let mut restarted = EventLog::open(dir.path(), &r.source_key()).unwrap();
        assert!(matches!(
            restarted.accept_recurring(&r, "", 120).unwrap(),
            RecurringTransition::Pending
        ));
        assert_eq!(restarted.read_rows().unwrap().unwrap().rows.len(), 1);
        ack(&mut restarted, seq, &ident, 120);
        force_compact(&mut restarted);
        let progress = restarted
            .recurring_lines()
            .unwrap()
            .into_iter()
            .find_map(|line| match line {
                Line::Known(record) if matches!(record.as_ref(), Record::TimerProgress { .. }) => {
                    Some(record)
                }
                _ => None,
            })
            .unwrap();
        // A later retention policy may omit every displayed old row: the
        // compacted proof must remain sufficient without those payloads.
        let mut encoded = serde_json::to_vec(progress.as_ref()).unwrap();
        encoded.push(b'\n');
        std::fs::write(&log.log_path, encoded).unwrap();
        let mut restarted = EventLog::open(dir.path(), &r.source_key()).unwrap();
        assert_eq!(restarted.load().unhandled.len(), 0);
        assert_eq!(restarted.next_seq, Some(seq + 1));
        let (next, _, _) = accepted(&mut restarted, &r, 130);
        assert_eq!(next, seq + 1);
    }

    #[test]
    fn malformed_ranges_are_refused_by_inspection_and_compaction() {
        let (_dir, r, mut log) = fixture();
        accepted(&mut log, &r, 120);
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&log.log_path).unwrap()).unwrap();
        value["item"]["recurrence"]["first_slot"] = 1.into();
        value["item"]["recurrence"]["count"] = 2.into();
        let mut encoded = serde_json::to_vec(&value).unwrap();
        encoded.push(b'\n');
        std::fs::write(&log.log_path, &encoded).unwrap();
        assert!(log.read_rows().is_err());
        assert!(log.accept_recurring(&r, "", 130).is_err());
        log.lines = COMPACT_AT;
        assert!(log.maybe_compact().is_err());
        assert_eq!(std::fs::read(&log.log_path).unwrap(), encoded);
    }

    #[test]
    fn missing_checkpoint_coverage_cannot_reopen_compacted_slots() {
        let (_dir, r, mut log) = fixture();
        let (seq, ident, _) = accepted(&mut log, &r, 120);
        ack(&mut log, seq, &ident, 150);
        force_compact(&mut log);
        let progress = log
            .recurring_lines()
            .unwrap()
            .into_iter()
            .find_map(|line| match line {
                Line::Known(record) if matches!(record.as_ref(), Record::TimerProgress { .. }) => {
                    Some(record)
                }
                _ => None,
            })
            .unwrap();
        let mut damaged = serde_json::to_value(progress.as_ref()).unwrap();
        damaged.as_object_mut().unwrap().remove("covered_through");
        let mut bytes = serde_json::to_vec(&damaged).unwrap();
        bytes.push(b'\n');
        std::fs::write(&log.log_path, &bytes).unwrap();
        assert!(log.accept_recurring(&r, "", 200).is_err());
        assert!(log.read_rows().is_err());
        assert_eq!(std::fs::read(&log.log_path).unwrap(), bytes);
    }
}
