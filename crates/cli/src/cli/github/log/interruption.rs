use super::*;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InterruptOutcome {
    Submitted,
    Unavailable,
    Failed,
    Indeterminate,
    #[serde(other)]
    Unknown,
}

/// An event's request to interrupt its agent, and the description its
/// source gave for the hint. Recorded with the event, so a later config
/// change never alters an accepted event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Interrupt {
    pub(crate) on: bool,
    pub(crate) hint: Option<String>,
}

impl Interrupt {
    /// The description is kept only for an event that interrupts.
    pub(crate) fn new(on: bool, hint: Option<&str>) -> Self {
        Self {
            on,
            hint: hint.filter(|_| on).map(str::to_owned),
        }
    }
}

impl From<bool> for Interrupt {
    fn from(on: bool) -> Self {
        Self::new(on, None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct InterruptAttempt {
    pub(crate) at: u64,
    /// None is a durable claim with an unconfirmed external outcome.
    pub(crate) outcome: Option<InterruptOutcome>,
}

impl EventLog {
    /// Claim exactly this logical row under the shared exclusive writer lock.
    /// The caller must obtain true before making any external side effect.
    pub(crate) fn claim_interrupt(&mut self, seq: u64, ident: &str) -> std::io::Result<bool> {
        let _lock = SidecarLock::acquire(&self.lock_path, true)?;
        let row = self.reply_target(seq, ident)?;
        if !row.interrupt || row.acked || row.interrupt_attempt.is_some() {
            // A prior complete append may have failed to sync. It still blocks
            // retry, and must be made durable before returning that decision.
            self.sync_existing_log()?;
            return Ok(false);
        }
        self.append_durable_unlocked(&Record::Interrupt {
            v: CURRENT_V,
            seq,
            ident: ident.into(),
            at: now_epoch(),
            outcome: None,
        })?;
        Ok(true)
    }

    pub(crate) fn finish_interrupt(
        &mut self,
        seq: u64,
        ident: &str,
        outcome: InterruptOutcome,
    ) -> std::io::Result<()> {
        let _lock = SidecarLock::acquire(&self.lock_path, true)?;
        let row = self.reply_target(seq, ident)?;
        let attempt = row
            .interrupt_attempt
            .ok_or_else(|| std::io::Error::other("interruption was never claimed"))?;
        if let Some(previous) = attempt.outcome {
            if previous != outcome {
                return Err(std::io::Error::other(
                    "interruption already has a different outcome",
                ));
            }
            return self.sync_existing_log();
        }
        self.append_durable_unlocked(&Record::Interrupt {
            v: CURRENT_V,
            seq,
            ident: ident.into(),
            at: now_epoch(),
            outcome: Some(outcome),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> WaitItem {
        WaitItem::Message {
            presentation: None,
            requires_reply: false,
            reply_route: None,
            handle: "id".into(),
            from: None,
            repo: None,
            subject: "hello".into(),
            body: "hello".into(),
            selector: None,
        }
    }
    fn fixture(interrupt: bool) -> (tempfile::TempDir, EventLog, u64, String) {
        let dir = tempfile::tempdir().unwrap();
        let mut log = EventLog::open(dir.path(), "messages").unwrap();
        log.load();
        let seq = log
            .append_interruptible_inbox(
                Transport::Message,
                None,
                Some("k"),
                None,
                &item(),
                interrupt.into(),
            )
            .unwrap();
        let ident = log.read_rows().unwrap().unwrap().rows[0].ident.clone();
        (dir, log, seq, ident)
    }

    #[test]
    fn claim_and_outcome_do_not_ack_or_change_backlog() {
        let (_dir, mut log, seq, ident) = fixture(true);
        assert!(log.claim_interrupt(seq, &ident).unwrap());
        assert!(!log.claim_interrupt(seq, &ident).unwrap());
        log.finish_interrupt(seq, &ident, InterruptOutcome::Submitted)
            .unwrap();
        let row = log.read_rows().unwrap().unwrap().rows.remove(0);
        assert!(!row.acked);
        assert_eq!(
            row.interrupt_attempt.unwrap().outcome,
            Some(InterruptOutcome::Submitted)
        );
        assert_eq!(log.load().unhandled.len(), 1);
    }

    #[test]
    fn unfinished_claim_survives_reload_and_compaction_without_retry() {
        let (dir, mut log, seq, ident) = fixture(true);
        assert!(log.claim_interrupt(seq, &ident).unwrap());
        log.lines = COMPACT_AT;
        log.maybe_compact().unwrap();
        let mut reopened = EventLog::open(dir.path(), "messages").unwrap();
        reopened.load();
        assert!(!reopened.claim_interrupt(seq, &ident).unwrap());
        assert_eq!(
            reopened.read_rows().unwrap().unwrap().rows[0]
                .interrupt_attempt
                .as_ref()
                .unwrap()
                .outcome,
            None
        );
    }

    #[test]
    fn ordinary_or_acked_events_never_claim_and_wrong_identity_is_refused() {
        let (_dir, mut ordinary, seq, ident) = fixture(false);
        assert!(!ordinary.claim_interrupt(seq, &ident).unwrap());
        let (_dir, mut log, seq, ident) = fixture(true);
        assert!(log.claim_interrupt(seq, "other-event").is_err());
        log.append_ack_ident(seq, &ident).unwrap();
        assert!(!log.claim_interrupt(seq, &ident).unwrap());
    }

    #[test]
    fn concurrent_writers_obtain_only_one_claim() {
        let (dir, _log, seq, ident) = fixture(true);
        let dir = dir.path().to_owned();
        let winners = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let dir = &dir;
                    let ident = &ident;
                    scope.spawn(move || {
                        EventLog::open(dir, "messages")
                            .unwrap()
                            .claim_interrupt(seq, ident)
                            .unwrap()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .filter(|won| *won)
                .count()
        });
        assert_eq!(winners, 1);
    }

    #[test]
    fn sync_failure_never_authorizes_effect_or_a_second_attempt() {
        let (_dir, mut log, seq, ident) = fixture(true);
        log.sync_fault.set(Some(DurableSync::File));
        assert!(log.claim_interrupt(seq, &ident).is_err());
        log.sync_fault.set(None);
        assert!(!log.claim_interrupt(seq, &ident).unwrap());
        assert!(!log.read_rows().unwrap().unwrap().rows[0].acked);
    }

    #[test]
    fn outcome_is_immutable_and_retained_with_recent_handled_rows() {
        let (_dir, mut log, seq, ident) = fixture(true);
        assert!(
            log.finish_interrupt(seq, &ident, InterruptOutcome::Submitted)
                .is_err()
        );
        assert!(log.claim_interrupt(seq, &ident).unwrap());
        log.finish_interrupt(seq, &ident, InterruptOutcome::Unavailable)
            .unwrap();
        log.finish_interrupt(seq, &ident, InterruptOutcome::Unavailable)
            .unwrap();
        assert!(
            log.finish_interrupt(seq, &ident, InterruptOutcome::Submitted)
                .is_err()
        );
        log.append_ack_ident(seq, &ident).unwrap();
        log.lines = COMPACT_AT;
        log.maybe_compact().unwrap();
        let row = log.read_rows().unwrap().unwrap().rows.remove(0);
        assert!(row.acked);
        assert_eq!(
            row.interrupt_attempt.unwrap().outcome,
            Some(InterruptOutcome::Unavailable)
        );
    }
}
