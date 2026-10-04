//! Wake evidence has no acknowledgement or backend-callback effects.
use super::*;
use clank_core::wait::EventPresentation;
use std::collections::{BTreeMap, BTreeSet};

impl EventLog {
    /// One lock/read/sync per source, however many selected copies were emitted.
    /// Exact identity prevents a reused sequence from receiving another wake.
    pub(crate) fn append_presentations(
        &mut self,
        selected: &[EventPresentation],
        at: u64,
        current: impl Fn() -> bool,
    ) -> std::io::Result<usize> {
        let _lock = SidecarLock::acquire(&self.lock_path, true)?;
        let read = self
            .read_rows_unlocked()?
            .ok_or_else(|| std::io::Error::other("corrupt presentation WAL"))?;
        let rows: BTreeMap<_, _> = read
            .rows
            .iter()
            .map(|row| ((row.seq, row.ident.as_str()), row))
            .collect();
        let mut records = Vec::new();
        let mut seen = BTreeSet::new();
        let mut repeated = false;
        for key in selected {
            if key.source != self.source_key() {
                continue;
            }
            if !seen.insert((key.seq, key.ident.as_str())) {
                continue;
            }
            let Some(row) = rows.get(&(key.seq, key.ident.as_str())) else {
                // Compaction/removal between selection and output is conservative.
                continue;
            };
            if row.baseline || row.item.event_view().is_none() {
                continue;
            }
            if row.woken_at.is_some() {
                repeated = true;
                continue;
            }
            records.push(Record::Presentation {
                v: CURRENT_V,
                seq: key.seq,
                ident: key.ident.clone(),
                at,
            });
        }
        if !current() {
            return Ok(0);
        }
        if !records.is_empty() {
            self.append_durable_batch_unlocked(&records)?;
        } else if repeated {
            // A previous fsync may have failed after its record became visible.
            self.sync_existing_log()?;
        }
        Ok(records.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn message(body: &str) -> WaitItem {
        WaitItem::Message {
            presentation: None,
            handle: body.into(),
            from: None,
            repo: None,
            subject: body.into(),
            body: body.into(),
            requires_reply: false,
            reply_route: None,
            selector: None,
        }
    }
    fn append(log: &mut EventLog, body: &str) -> EventPresentation {
        let item = message(body);
        let seq = log
            .append_inbox(Transport::Message, Some(body), None, None, &item)
            .unwrap();
        log.presentation_key(seq, Transport::Message, Some(body), None, None, &item)
    }
    #[test]
    fn first_wake_is_exact_durable_and_never_acknowledges() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = EventLog::open(dir.path(), "messages").unwrap();
        let a = append(&mut log, "first");
        let b = append(&mut log, "second");
        let before = std::fs::read(&log.log_path).unwrap();
        let read = log.read_rows().unwrap().unwrap();
        assert!(read.rows.iter().all(|r| r.woken_at.is_none() && !r.acked));
        assert_eq!(
            std::fs::read(&log.log_path).unwrap(),
            before,
            "inspection is not presentation"
        );
        assert_eq!(
            log.append_presentations(&[a.clone(), a.clone(), b.clone()], 100, || true)
                .unwrap(),
            2
        );
        let size = std::fs::metadata(&log.log_path).unwrap().len();
        assert_eq!(
            log.append_presentations(&[a.clone(), b.clone()], 50, || true)
                .unwrap(),
            0
        );
        assert_eq!(
            std::fs::metadata(&log.log_path).unwrap().len(),
            size,
            "repeat wakes append nothing"
        );
        let mut reopened = EventLog::open(dir.path(), "messages").unwrap();
        let rows = reopened.read_rows().unwrap().unwrap().rows;
        assert!(rows.iter().all(|r| r.woken_at == Some(100) && !r.acked));
        assert_eq!(reopened.load().unhandled.len(), 2);
        reopened.append_checked_ack(a.seq, &a.ident).unwrap();
        let rows = reopened.read_rows().unwrap().unwrap().rows;
        assert!(rows[0].acked && !rows[1].acked);
        assert_eq!(rows[0].woken_at, Some(100));
    }
    #[test]
    fn missing_identity_wrong_source_and_replaced_binding_cannot_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = EventLog::open(dir.path(), "messages").unwrap();
        let a = append(&mut log, "a");
        for key in [
            EventPresentation {
                ident: "f:other".into(),
                ..a.clone()
            },
            EventPresentation {
                source: "other".into(),
                ..a.clone()
            },
            EventPresentation {
                seq: a.seq + 1,
                ..a.clone()
            },
        ] {
            assert_eq!(log.append_presentations(&[key], 100, || true).unwrap(), 0);
        }
        assert_eq!(log.append_presentations(&[a], 100, || false).unwrap(), 0);
        assert!(log.read_rows().unwrap().unwrap().rows[0].woken_at.is_none());
    }
    #[test]
    fn colliding_sequences_and_compaction_retain_only_the_selected_copy() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = EventLog::open(dir.path(), "messages").unwrap();
        let a = append(&mut log, "a");
        let item = message("collision");
        log.append(&Record::Inbox {
            v: CURRENT_V,
            seq: a.seq,
            at: 0,
            event_at: None,
            baseline: false,
            interrupt: false,
            transport: Transport::Message,
            feed_id: Some("collision".into()),
            action_key: None,
            item,
        })
        .unwrap();
        log.append_presentations(&[a.clone()], 100, || true)
            .unwrap();
        log.append_checked_ack(a.seq, &a.ident).unwrap();
        for n in 0..DISPLAY_CAP {
            let key = append(&mut log, &format!("handled-{n}"));
            log.append_presentations(&[key.clone()], 200, || true)
                .unwrap();
            log.append_checked_ack(key.seq, &key.ident).unwrap();
        }
        log.lines = COMPACT_AT;
        log.maybe_compact().unwrap();
        let rows = log.read_rows().unwrap().unwrap().rows;
        assert_eq!(rows.len(), DISPLAY_CAP + 1);
        let collision = rows.iter().find(|r| r.seq == a.seq).unwrap();
        assert_eq!(collision.ident, "f:collision");
        assert!(!collision.acked && collision.woken_at.is_none());
        assert!(
            rows.iter()
                .filter(|r| r.seq != a.seq)
                .all(|r| r.woken_at == Some(200) && r.acked)
        );
        let text = std::fs::read_to_string(&log.log_path).unwrap();
        assert_eq!(
            text.lines()
                .filter(|l| l.contains("\"t\":\"presentation\""))
                .count(),
            DISPLAY_CAP
        );
        assert_eq!(log.append_presentations(&[a], 300, || true).unwrap(), 0);
    }
    #[test]
    fn uncertain_sync_is_reconciled_before_suppressing_a_retry() {
        for boundary in [DurableSync::File, DurableSync::Directory] {
            let dir = tempfile::tempdir().unwrap();
            let mut log = EventLog::open(dir.path(), "messages").unwrap();
            let key = append(&mut log, "a");
            log.sync_fault.set(Some(boundary));
            assert!(
                log.append_presentations(&[key.clone()], 100, || true)
                    .is_err()
            );
            log.sync_fault.set(Some(boundary));
            assert!(
                log.append_presentations(&[key.clone()], 200, || true)
                    .is_err()
            );
            log.sync_fault.set(None);
            assert_eq!(log.append_presentations(&[key], 300, || true).unwrap(), 0);
            let rows = log.read_rows().unwrap().unwrap().rows;
            assert_eq!(rows[0].woken_at, Some(100));
            assert!(!rows[0].acked);
        }
    }
}
