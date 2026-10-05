//! Durable message operations and receiver custody. No cross-repo lock or worker.
use super::*;
use clank_core::{ids::AgentLabel, wait::MessageReplyRoute};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const DELIVERY_PREFIX: &str = "message-reply/";
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct MessageOperation {
    pub id: String,
    pub repo: PathBuf,
    pub author: AgentLabel,
    pub original_seq: u64,
    pub original_ident: String,
    pub original_handle: String,
    pub route: MessageReplyRoute,
    pub text: String,
    pub at: u64,
    pub acknowledge: bool,
}
impl MessageOperation {
    pub(crate) fn delivery_id(&self) -> String {
        blake3::hash(
            &serde_json::to_vec(&("message-delivery-v1", &self.repo, &self.author, &self.id))
                .unwrap(),
        )
        .to_hex()
        .to_string()
    }
    pub(crate) fn item(&self) -> WaitItem {
        WaitItem::Message {
            presentation: None,
            handle: self.delivery_id(),
            from: Some(self.author.to_string()),
            repo: Some(self.repo.display().to_string()),
            subject: self
                .text
                .lines()
                .find(|s| !s.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_string(),
            body: self.text.clone(),
            requires_reply: false,
            reply_route: None,
            selector: None,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct MessageReceipt {
    pub id: String,
    pub digest: String,
    pub seq: u64,
    pub ident: String,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct MessageOperationState {
    pub seq: u64,
    pub operation: MessageOperation,
    pub receipt: Option<MessageReceipt>,
}
fn invalid(text: &str) -> std::io::Error {
    std::io::Error::other(text)
}
fn digest(item: &WaitItem) -> String {
    blake3::hash(&serde_json::to_vec(item).unwrap())
        .to_hex()
        .to_string()
}
pub(super) fn operations(lines: &[Line]) -> std::io::Result<Vec<MessageOperationState>> {
    let mut states: BTreeMap<String, MessageOperationState> = BTreeMap::new();
    for line in lines {
        if let Line::Known(record) = line
            && let Record::MessageOperation {
                seq,
                operation,
                receipt,
                ..
            } = record.as_ref()
        {
            let state =
                states
                    .entry(operation.id.clone())
                    .or_insert_with(|| MessageOperationState {
                        seq: *seq,
                        operation: operation.clone(),
                        receipt: None,
                    });
            if state.seq != *seq
                || state.operation != *operation
                || (state.receipt.is_some() && receipt.is_some() && state.receipt != *receipt)
            {
                return Err(invalid(
                    "conflicting message operation records; repair required",
                ));
            }
            if let Some(receipt) = receipt {
                if receipt.id != operation.delivery_id()
                    || receipt.digest != digest(&operation.item())
                {
                    return Err(invalid(
                        "message acceptance does not match its immutable operation",
                    ));
                }
                state.receipt = Some(receipt.clone());
            }
        }
    }
    let mut out: Vec<_> = states.into_values().collect();
    out.sort_by_key(|s| s.seq);
    Ok(out)
}
pub(super) fn operation_acks(lines: &[Line]) -> Vec<(u64, Option<String>)> {
    // Corrupt/conflicting transactions never manufacture handled work. Explicit
    // mutation paths fail with the underlying repair error.
    operations(lines)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.operation.acknowledge && s.receipt.is_some())
        .map(|s| (s.operation.original_seq, Some(s.operation.original_ident)))
        .collect()
}
fn receipts(lines: &[Line]) -> std::io::Result<BTreeMap<String, (MessageReceipt, bool)>> {
    let mut states: BTreeMap<String, (MessageReceipt, bool)> = BTreeMap::new();
    for line in lines {
        let Line::Known(record) = line else { continue };
        let entry = match record.as_ref() {
            Record::Inbox {
                seq,
                action_key: Some(key),
                item,
                event_at,
                transport: Transport::Message,
                ..
            } if key.starts_with(DELIVERY_PREFIX) => {
                let id = key.strip_prefix(DELIVERY_PREFIX).unwrap().to_string();
                let ident = logical_ident(None, Some(key), *event_at, Transport::Message, item);
                Some((
                    MessageReceipt {
                        id,
                        digest: digest(item),
                        seq: *seq,
                        ident,
                    },
                    false,
                ))
            }
            Record::MessageReceipt {
                receipt, released, ..
            } => Some((receipt.clone(), *released)),
            _ => None,
        };
        if let Some((receipt, released)) = entry {
            if let Some((saved, done)) = states.get_mut(&receipt.id) {
                if *saved != receipt {
                    return Err(invalid(
                        "conflicting message delivery receipts; repair required",
                    ));
                }
                *done |= released;
            } else {
                states.insert(receipt.id.clone(), (receipt, released));
            }
        }
    }
    Ok(states)
}
impl EventLog {
    fn message_lines(&self) -> std::io::Result<Vec<Line>> {
        if self.log_path.file_stem().and_then(|s| s.to_str()) != Some("messages") {
            return Err(invalid("message transactions require the messages WAL"));
        }
        let text = match std::fs::read_to_string(&self.log_path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        Self::parse(&text).map_err(|_| invalid("message WAL is corrupt; refusing transaction"))
    }
    pub(crate) fn message_operations(&self) -> std::io::Result<Vec<MessageOperationState>> {
        let _lock = SidecarLock::acquire(&self.lock_path, false)?;
        operations(&self.message_lines()?)
    }
    /// Read-only lookup before membership validation: an accepted delivery
    /// remains accepted when its recipient is subsequently removed.
    pub(crate) fn message_delivery_receipt(
        &self,
        operation: &MessageOperation,
    ) -> std::io::Result<Option<MessageReceipt>> {
        let _lock = SidecarLock::acquire(&self.lock_path, false)?;
        let saved = receipts(&self.message_lines()?)?.remove(&operation.delivery_id());
        if let Some((receipt, _)) = saved {
            if receipt.digest != digest(&operation.item()) {
                return Err(invalid("delivery identity has different message input"));
            }
            Ok(Some(receipt))
        } else {
            Ok(None)
        }
    }
    /// The caller owns the author's dispatch lease and message writer lock.
    pub(crate) fn begin_message_operation(
        &mut self,
        operation: &MessageOperation,
    ) -> std::io::Result<MessageOperationState> {
        let _lock = SidecarLock::acquire(&self.lock_path, true)?;
        let lines = self.message_lines()?;
        let states = operations(&lines)?;
        if let Some(state) = states.iter().find(|s| s.operation.id == operation.id) {
            if state.operation != *operation {
                return Err(invalid("operation already has different immutable input"));
            }
            self.sync_existing_log()?;
            return Ok(state.clone());
        }
        if operation.text.trim().is_empty()
            || operation.id.is_empty()
            || !operation.repo.is_absolute()
            || !operation.route.repo.is_absolute()
        {
            return Err(invalid(
                "message operation needs a nonempty ID/body and absolute repositories",
            ));
        }
        let row = self.reply_target(operation.original_seq, &operation.original_ident)?;
        if !matches!(&row.item, WaitItem::Message { handle, .. } if handle == &operation.original_handle)
        {
            return Err(invalid("message operation's original copy changed"));
        }
        if operation.acknowledge && row.acked {
            return Err(invalid(
                "message was already acknowledged; use a deliberate reply",
            ));
        }
        if operation.acknowledge
            && states.iter().any(|s| {
                s.operation.acknowledge
                    && s.operation.original_seq == operation.original_seq
                    && s.operation.original_ident == operation.original_ident
            })
        {
            return Err(invalid(
                "an acknowledgement reply is already recorded for this copy",
            ));
        }
        let seq = Self::fold(lines)
            .max_seq
            .checked_add(1)
            .ok_or_else(|| invalid("message sequence namespace exhausted"))?;
        self.append_durable_unlocked(&Record::MessageOperation {
            v: CURRENT_V,
            seq,
            operation: operation.clone(),
            receipt: None,
        })?;
        self.next_seq = seq.checked_add(1);
        Ok(MessageOperationState {
            seq,
            operation: operation.clone(),
            receipt: None,
        })
    }
    /// Caller holds this recipient's message writer lock, like ordinary send;
    /// it is never held across another repo operation. The inbox record itself
    /// is acceptance: compaction synthesizes its pin before aging the body out.
    pub(crate) fn accept_message_delivery(
        &mut self,
        operation: &MessageOperation,
    ) -> std::io::Result<MessageReceipt> {
        let _lock = SidecarLock::acquire(&self.lock_path, true)?;
        let lines = self.message_lines()?;
        let id = operation.delivery_id();
        let item = operation.item();
        if let Some((receipt, _)) = receipts(&lines)?.get(&id) {
            if receipt.digest != digest(&item) {
                return Err(invalid("delivery identity has different message input"));
            }
            self.sync_existing_log()?;
            return Ok(receipt.clone());
        }
        let seq = Self::fold(lines)
            .max_seq
            .checked_add(1)
            .ok_or_else(|| invalid("message sequence namespace exhausted"))?;
        let key = format!("{DELIVERY_PREFIX}{id}");
        let receipt = MessageReceipt {
            id,
            digest: digest(&item),
            seq,
            ident: logical_ident(
                None,
                Some(&key),
                Some(operation.at),
                Transport::Message,
                &item,
            ),
        };
        self.append_durable_unlocked(&Record::Inbox {
            v: CURRENT_V,
            seq,
            at: now_epoch(),
            event_at: Some(operation.at),
            baseline: false,
            interrupt: false,
            interrupt_hint: None,
            transport: Transport::Message,
            feed_id: None,
            action_key: Some(key),
            item,
        })?;
        self.next_seq = seq.checked_add(1);
        Ok(receipt)
    }
    pub(crate) fn accept_message_operation(
        &mut self,
        state: &MessageOperationState,
        receipt: &MessageReceipt,
    ) -> std::io::Result<()> {
        let _lock = SidecarLock::acquire(&self.lock_path, true)?;
        let current = operations(&self.message_lines()?)?
            .into_iter()
            .find(|s| s.operation.id == state.operation.id)
            .ok_or_else(|| invalid("message intent disappeared; refusing acceptance"))?;
        if current.seq != state.seq
            || current.operation != state.operation
            || receipt.id != state.operation.delivery_id()
            || receipt.digest != digest(&state.operation.item())
        {
            return Err(invalid(
                "message acceptance does not match its immutable intent",
            ));
        }
        if let Some(saved) = current.receipt {
            if saved != *receipt {
                return Err(invalid("conflicting message acceptance"));
            }
            return self.sync_existing_log();
        }
        self.reply_target(
            state.operation.original_seq,
            &state.operation.original_ident,
        )?;
        self.append_durable_unlocked(&Record::MessageOperation {
            v: CURRENT_V,
            seq: state.seq,
            operation: state.operation.clone(),
            receipt: Some(receipt.clone()),
        })
    }
    /// Only the dispatcher calls this AFTER durable source acceptance.
    pub(crate) fn release_message_receipt(
        &mut self,
        receipt: &MessageReceipt,
    ) -> std::io::Result<()> {
        let _lock = SidecarLock::acquire(&self.lock_path, true)?;
        if let Some((saved, released)) = receipts(&self.message_lines()?)?.get(&receipt.id) {
            if saved != receipt {
                return Err(invalid("message receipt changed; refusing release"));
            }
            if *released {
                return self.sync_existing_log();
            }
            self.append_durable_unlocked(&Record::MessageReceipt {
                v: CURRENT_V,
                seq: receipt.seq,
                receipt: receipt.clone(),
                released: true,
            })?;
        }
        Ok(())
    }
}
/// Retain every unaccepted operation, recent accepted history, and ack decisions
/// while their original is selectable. Pins are independent of display retention.
pub(super) fn compact_records(
    lines: &[Line],
    targets: &BTreeSet<(u64, String)>,
) -> std::io::Result<Vec<Record>> {
    let states = operations(lines)?;
    let newest: BTreeSet<_> = states
        .iter()
        .rev()
        .filter(|s| s.receipt.is_some())
        .take(DISPLAY_CAP)
        .map(|s| s.operation.id.clone())
        .collect();
    let keep = |s: &MessageOperationState| {
        s.receipt.is_none()
            || newest.contains(&s.operation.id)
            || (s.operation.acknowledge
                && targets
                    .contains(&(s.operation.original_seq, s.operation.original_ident.clone())))
    };
    let mut pruned: BTreeSet<_> = lines
        .iter()
        .filter_map(|line| match line {
            Line::Known(record) => match record.as_ref() {
                Record::MessageHistoryPruned { seq, ident, .. }
                    if targets.contains(&(*seq, ident.clone())) =>
                {
                    Some((*seq, ident.clone()))
                }
                _ => None,
            },
            _ => None,
        })
        .collect();
    for state in &states {
        let key = (
            state.operation.original_seq,
            state.operation.original_ident.clone(),
        );
        if !keep(state) && targets.contains(&key) {
            pruned.insert(key);
        }
    }
    let mut records: Vec<_> = states
        .into_iter()
        .filter(keep)
        .map(|s| Record::MessageOperation {
            v: CURRENT_V,
            seq: s.seq,
            operation: s.operation,
            receipt: s.receipt,
        })
        .collect();
    records.extend(
        pruned
            .into_iter()
            .map(|(seq, ident)| Record::MessageHistoryPruned {
                v: CURRENT_V,
                seq,
                ident,
            }),
    );
    for (_, (receipt, released)) in receipts(lines)? {
        if !released || targets.contains(&(receipt.seq, receipt.ident.clone())) {
            records.push(Record::MessageReceipt {
                v: CURRENT_V,
                seq: receipt.seq,
                receipt,
                released,
            });
        }
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(
        acknowledge: bool,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        EventLog,
        EventLog,
        MessageOperation,
    ) {
        let origin = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let mut source = EventLog::open(origin.path(), "messages").unwrap();
        let route = MessageReplyRoute {
            repo: target.path().into(),
            agent: AgentLabel::parse("coordinator").unwrap(),
        };
        let original = WaitItem::Message {
            presentation: None,
            handle: "original".into(),
            from: None,
            repo: None,
            subject: "question".into(),
            body: "question".into(),
            requires_reply: true,
            reply_route: Some(route.clone()),
            selector: None,
        };
        source
            .append_inbox(
                Transport::Message,
                None,
                Some("original-copy"),
                None,
                &original,
            )
            .unwrap();
        let row = source.read_rows().unwrap().unwrap().rows.remove(0);
        let operation = MessageOperation {
            id: "first".into(),
            repo: origin.path().into(),
            author: AgentLabel::parse("worker").unwrap(),
            original_seq: row.seq,
            original_ident: row.ident,
            original_handle: "original".into(),
            route,
            text: "  Answer β.\nKeep this whitespace.  ".into(),
            at: 42,
            acknowledge,
        };
        let destination = EventLog::open(target.path(), "messages").unwrap();
        (origin, target, source, destination, operation)
    }
    fn compact(log: &mut EventLog) {
        log.lines = COMPACT_AT;
        log.maybe_compact().unwrap();
    }
    #[test]
    fn retained_parent_reports_pruned_reply_history_across_repeated_compaction() {
        let (_source_dir, _target_dir, mut source, mut target, mut operation) = fixture(false);
        for n in 0..=DISPLAY_CAP {
            operation.id = format!("history-{n}");
            let state = source.begin_message_operation(&operation).unwrap();
            let receipt = target.accept_message_delivery(&operation).unwrap();
            source.accept_message_operation(&state, &receipt).unwrap();
            target.release_message_receipt(&receipt).unwrap();
        }
        let before = source.read_rows().unwrap().unwrap().rows.remove(0);
        assert_eq!(before.message_replies.len(), DISPLAY_CAP + 1);
        assert!(!before.message_history_pruned && !before.acked);
        for _ in 0..2 {
            source.lines = COMPACT_AT;
            source.maybe_compact().unwrap();
            let after = source.read_rows().unwrap().unwrap().rows.remove(0);
            assert!(after.message_history_pruned && !after.acked);
            assert_eq!(after.message_replies.len(), DISPLAY_CAP);
            assert_eq!(
                serde_json::to_value(after).unwrap()["message_history_pruned"],
                true
            );
        }
    }

    #[test]
    fn acceptance_is_one_source_ack_decision_and_operations_never_become_sender_work() {
        let (_origin, _target, mut source, mut target, op) = fixture(true);
        let state = source.begin_message_operation(&op).unwrap();
        assert_eq!(source.load().unhandled.len(), 1);
        assert!(
            source
                .append_checked_ack(op.original_seq, &op.original_ident)
                .is_err()
        );
        let receipt = target.accept_message_delivery(&op).unwrap();
        assert!(!source.read_rows().unwrap().unwrap().rows[0].acked);
        assert_eq!(target.accept_message_delivery(&op).unwrap(), receipt);
        let incoming = target.read_rows().unwrap().unwrap().rows.remove(0);
        assert_eq!(incoming.item.event_view().unwrap().body.unwrap(), op.text);
        assert!(
            !incoming.item.event_view().unwrap().requires_reply,
            "replies do not inherit obligations"
        );
        source.accept_message_operation(&state, &receipt).unwrap();
        assert!(source.load().unhandled.is_empty());
        assert!(source.read_rows().unwrap().unwrap().rows[0].acked);
        source.accept_message_operation(&state, &receipt).unwrap();
        let mut changed = op.clone();
        changed.text = "different".into();
        assert!(source.begin_message_operation(&changed).is_err());
        target.release_message_receipt(&receipt).unwrap();
        compact(&mut source);
        assert!(source.read_rows().unwrap().unwrap().rows[0].acked);
        assert_eq!(source.message_operations().unwrap().len(), 1);
        assert_eq!(target.read_rows().unwrap().unwrap().rows.len(), 1);
    }
    #[test]
    fn deliberate_equal_body_replies_have_distinct_identity_without_acknowledgement() {
        let (_origin, _target, mut source, mut target, op) = fixture(false);
        for id in ["first", "second"] {
            let mut next = op.clone();
            next.id = id.into();
            let state = source.begin_message_operation(&next).unwrap();
            let receipt = target.accept_message_delivery(&next).unwrap();
            source.accept_message_operation(&state, &receipt).unwrap();
            target.release_message_receipt(&receipt).unwrap();
        }
        assert_eq!(target.read_rows().unwrap().unwrap().rows.len(), 2);
        assert_eq!(source.message_operations().unwrap().len(), 2);
        assert!(!source.read_rows().unwrap().unwrap().rows[0].acked);
        assert_eq!(source.load().unhandled.len(), 1);
        compact(&mut source);
        assert_eq!(source.message_operations().unwrap().len(), 2);
    }
    #[test]
    fn receiver_pin_survives_ack_and_body_pruning_until_source_acceptance() {
        let (_origin, target_dir, mut source, mut target, op) = fixture(true);
        let state = source.begin_message_operation(&op).unwrap();
        let receipt = target.accept_message_delivery(&op).unwrap();
        target
            .append_checked_ack(receipt.seq, &receipt.ident)
            .unwrap();
        for i in 0..=DISPLAY_CAP {
            target
                .append_baseline_inbox(Some(&format!("newer-{i}")), None, None, &op.item())
                .unwrap();
        }
        compact(&mut target);
        assert!(
            target
                .read_rows()
                .unwrap()
                .unwrap()
                .rows
                .iter()
                .all(|r| r.seq != receipt.seq)
        );
        let mut restarted = EventLog::open(target_dir.path(), "messages").unwrap();
        assert_eq!(restarted.accept_message_delivery(&op).unwrap(), receipt);
        assert_eq!(
            restarted.read_rows().unwrap().unwrap().rows.len(),
            DISPLAY_CAP
        );
        source.accept_message_operation(&state, &receipt).unwrap();
        restarted.release_message_receipt(&receipt).unwrap();
        compact(&mut restarted);
        assert!(
            receipts(&restarted.message_lines().unwrap())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            source.begin_message_operation(&op).unwrap().receipt,
            Some(receipt)
        );
    }
    #[test]
    fn pending_ack_on_an_ordinary_message_blocks_plain_ack_but_progress_does_not() {
        for ack in [true, false] {
            let (origin, _target, mut source, _destination, mut op) = fixture(ack);
            let original = WaitItem::Message {
                presentation: None,
                handle: "ordinary".into(),
                from: None,
                repo: None,
                subject: "hello".into(),
                body: "hello".into(),
                requires_reply: false,
                reply_route: None,
                selector: None,
            };
            source
                .append_inbox(Transport::Message, None, Some("ordinary"), None, &original)
                .unwrap();
            let row = source.read_rows().unwrap().unwrap().rows.pop().unwrap();
            op.original_seq = row.seq;
            op.original_ident = row.ident;
            op.original_handle = "ordinary".into();
            source.begin_message_operation(&op).unwrap();
            let mut restarted = EventLog::open(origin.path(), "messages").unwrap();
            assert_eq!(
                restarted
                    .append_checked_ack(op.original_seq, &op.original_ident)
                    .is_err(),
                ack
            );
            if ack {
                compact(&mut restarted);
                assert!(
                    restarted
                        .append_checked_ack(op.original_seq, &op.original_ident)
                        .is_err()
                );
            }
        }
    }
    #[test]
    fn uncertain_sync_is_reconciled_before_retry_and_does_not_duplicate_delivery() {
        for point in [DurableSync::File, DurableSync::Directory] {
            let (origin, target_dir, mut source, mut target, op) = fixture(true);
            source.sync_fault.set(Some(point));
            assert!(source.begin_message_operation(&op).is_err());
            assert!(target.read_rows().unwrap().unwrap().rows.is_empty());
            let mut source = EventLog::open(origin.path(), "messages").unwrap();
            let state = source.begin_message_operation(&op).unwrap();
            target.sync_fault.set(Some(point));
            assert!(target.accept_message_delivery(&op).is_err());
            assert!(!source.read_rows().unwrap().unwrap().rows[0].acked);
            let mut target = EventLog::open(target_dir.path(), "messages").unwrap();
            let receipt = target.accept_message_delivery(&op).unwrap();
            assert_eq!(target.read_rows().unwrap().unwrap().rows.len(), 1);
            source.sync_fault.set(Some(point));
            assert!(source.accept_message_operation(&state, &receipt).is_err());
            let mut source = EventLog::open(origin.path(), "messages").unwrap();
            source.accept_message_operation(&state, &receipt).unwrap();
            assert!(source.read_rows().unwrap().unwrap().rows[0].acked);
            target.sync_fault.set(Some(point));
            assert!(target.release_message_receipt(&receipt).is_err());
            let mut target = EventLog::open(target_dir.path(), "messages").unwrap();
            target.release_message_receipt(&receipt).unwrap();
        }
    }

    #[test]
    fn acceptance_discriminates_colliding_copy_sequences_through_compaction() {
        let (_origin, _target, mut source, mut target, op) = fixture(true);
        source
            .append(&Record::Inbox {
                v: CURRENT_V,
                seq: op.original_seq,
                at: 0,
                event_at: None,
                baseline: false,
                interrupt: false,
                interrupt_hint: None,
                transport: Transport::Message,
                feed_id: None,
                action_key: Some("other-copy".into()),
                item: op.item(),
            })
            .unwrap();
        let state = source.begin_message_operation(&op).unwrap();
        let receipt = target.accept_message_delivery(&op).unwrap();
        source.accept_message_operation(&state, &receipt).unwrap();
        compact(&mut source);
        let rows = source.read_rows().unwrap().unwrap().rows;
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .find(|r| r.ident == op.original_ident)
                .unwrap()
                .acked
        );
        assert!(
            !rows
                .iter()
                .find(|r| r.ident != op.original_ident)
                .unwrap()
                .acked
        );
        assert_eq!(source.load().unhandled.len(), 1);
    }

    #[test]
    fn torn_transaction_tails_are_repaired_before_recovery_appends() {
        let (_origin, _target, mut source, mut target, op) = fixture(true);
        let state = source.begin_message_operation(&op).unwrap();
        let receipt = target.accept_message_delivery(&op).unwrap();
        for log in [&source, &target] {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&log.log_path)
                .unwrap()
                .write_all(b"{\"t\":\"message")
                .unwrap();
        }
        assert_eq!(target.accept_message_delivery(&op).unwrap(), receipt);
        source.accept_message_operation(&state, &receipt).unwrap();
        target.release_message_receipt(&receipt).unwrap();
        assert!(source.read_rows().unwrap().unwrap().rows[0].acked);
        assert_eq!(target.read_rows().unwrap().unwrap().rows.len(), 1);
        assert!(!source.load().log_quarantined);
        assert!(!target.load().log_quarantined);
    }

    #[test]
    fn pending_progress_keeps_an_already_handled_original_available_until_acceptance() {
        let (_origin, _target, mut source, mut target, op) = fixture(false);
        source
            .append_ack_ident(op.original_seq, &op.original_ident)
            .unwrap();
        let state = source.begin_message_operation(&op).unwrap();
        for i in 0..=DISPLAY_CAP {
            source
                .append_baseline_inbox(Some(&format!("newer-{i}")), None, None, &op.item())
                .unwrap();
        }
        compact(&mut source);
        assert!(
            source
                .read_rows()
                .unwrap()
                .unwrap()
                .rows
                .iter()
                .any(|r| r.ident == op.original_ident)
        );
        let receipt = target.accept_message_delivery(&op).unwrap();
        source.accept_message_operation(&state, &receipt).unwrap();
        compact(&mut source);
        assert!(
            source
                .read_rows()
                .unwrap()
                .unwrap()
                .rows
                .iter()
                .all(|r| r.ident != op.original_ident)
        );
        assert_eq!(
            source.message_operations().unwrap()[0].receipt,
            Some(receipt)
        );
    }
}
