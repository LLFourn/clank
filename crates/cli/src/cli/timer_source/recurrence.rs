//! Pure anchored recurrence calculations. No clock reads, loops over ticks,
//! files, tasks or completion state; the generation WAL supplies coverage.

use clank_core::agent_config::{TimerGeneration, TimerSource};

pub(super) fn interval(source: &TimerSource) -> anyhow::Result<Option<u64>> {
    let Some(raw) = source.repeat_every.as_deref() else {
        return Ok(None);
    };
    let duration = super::super::wait::parse_duration_str(raw)
        .map_err(|error| anyhow::anyhow!("timer `{}` repeat_every: {error}", source.name))?
        .ok_or_else(|| anyhow::anyhow!("timer `{}` repeat_every must be positive", source.name))?;
    anyhow::ensure!(
        !duration.is_zero() && duration.subsec_nanos() == 0,
        "timer `{}` repeat_every must be positive whole seconds",
        source.name
    );
    Ok(Some(duration.as_secs()))
}

/// A fixed-size acceptance snapshot, regardless of how long Clank was offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slots {
    pub(crate) first: u64,
    pub(crate) last: u64,
    pub(crate) count: u64,
    pub(crate) due_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Recurrence {
    pub(crate) generation: TimerGeneration,
    pub(crate) id: String,
}

impl Recurrence {
    pub(crate) fn parse(source: &TimerSource, scope: Option<&str>) -> anyhow::Result<Self> {
        let anchor = super::parse(source)?;
        let interval = interval(source)?
            .ok_or_else(|| anyhow::anyhow!("timer `{}` is not recurring", source.name))?;
        Self::from_generation(TimerGeneration {
            name: source.name.clone(),
            scope: scope.map(str::to_owned),
            anchor,
            interval,
        })
    }

    /// Validate persisted descriptors by the same rules as fresh definitions.
    pub(crate) fn from_generation(generation: TimerGeneration) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !generation.name.trim().is_empty(),
            "empty recurring timer name"
        );
        anyhow::ensure!(generation.interval > 0, "zero recurring timer interval");
        time::OffsetDateTime::from_unix_timestamp(i64::try_from(generation.anchor)?)
            .map_err(|error| anyhow::anyhow!("invalid recurring anchor: {error}"))?;
        anyhow::ensure!(
            generation.anchor.checked_add(generation.interval).is_some(),
            "recurring timer exceeds the timestamp range"
        );
        anyhow::ensure!(
            generation
                .scope
                .as_ref()
                .is_none_or(|scope| !scope.is_empty()),
            "empty recurring timer invocation scope"
        );
        let encoded = serde_json::to_vec(&(
            "clank-timer-recurrence-v1",
            &generation.name,
            &generation.scope,
            generation.anchor,
            generation.interval,
        ))?;
        let id = blake3::hash(&encoded).to_hex().to_string();
        Ok(Self { generation, id })
    }

    pub(crate) fn source_key(&self) -> String {
        let origin = if self.generation.scope.is_some() {
            "invocation"
        } else {
            "config"
        };
        format!("timer-{origin}-recurring-{}", self.id)
    }

    pub(crate) fn producer_id(&self, slot: u64) -> anyhow::Result<String> {
        self.due_at(slot)?;
        Ok(format!("timer-recurring/{}/{slot}", self.id))
    }

    pub(crate) fn latest(&self, wall: u64) -> Option<u64> {
        wall.checked_sub(self.generation.anchor)
            .map(|elapsed| elapsed / self.generation.interval)
    }

    pub(crate) fn due_at(&self, slot: u64) -> anyhow::Result<u64> {
        slot.checked_mul(self.generation.interval)
            .and_then(|elapsed| self.generation.anchor.checked_add(elapsed))
            .ok_or_else(|| anyhow::anyhow!("recurring timer slot exceeds the timestamp range"))
    }

    /// Call only when the WAL proves there is no pending obligation.
    pub(crate) fn catch_up(
        &self,
        covered: Option<u64>,
        wall: u64,
    ) -> anyhow::Result<Option<Slots>> {
        let first = self.first_after(covered)?;
        let Some(last) = self.latest(wall).filter(|last| *last >= first) else {
            return Ok(None);
        };
        let count = last
            .checked_sub(first)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| {
                anyhow::anyhow!("recurring timer missed count exceeds the index range")
            })?;
        Ok(Some(Slots {
            first,
            last,
            count,
            due_at: self.due_at(last)?,
        }))
    }

    /// May be overdue: the owner transacts catch-up before arming this target.
    pub(crate) fn next_due(&self, covered: Option<u64>) -> anyhow::Result<u64> {
        self.due_at(self.first_after(covered)?)
    }

    fn first_after(&self, covered: Option<u64>) -> anyhow::Result<u64> {
        match covered {
            None => Ok(0),
            Some(slot) => {
                self.due_at(slot)?;
                slot.checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("recurring timer index exhausted"))
            }
        }
    }

    /// Ack covers pending ticks through its recorded timestamp, and never rolls
    /// back the accepted snapshot or prior coverage when the wall clock retreats.
    pub(crate) fn ack_coverage(
        &self,
        previous: Option<u64>,
        accepted: u64,
        at: u64,
    ) -> anyhow::Result<u64> {
        self.due_at(accepted)?;
        if let Some(previous) = previous {
            self.due_at(previous)?;
        }
        Ok(previous
            .into_iter()
            .chain(self.latest(at))
            .fold(accepted, u64::max))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> TimerSource {
        TimerSource {
            name: "check/deploy 🦀".into(),
            due_at: "1970-01-01T00:01:40Z".into(),
            repeat_every: Some("10s".into()),
            content: Some("check".into()),
        }
    }

    #[test]
    fn equivalent_definitions_share_generation_but_scopes_and_edits_do_not() {
        let mut a = source();
        a.repeat_every = Some("1h".into());
        let original = Recurrence::parse(&a, None).unwrap();
        a.due_at = "1970-01-01T11:01:40+11:00".into();
        a.repeat_every = Some(" 60m ".into());
        a.content = Some("new instructions".into());
        assert_eq!(Recurrence::parse(&a, None).unwrap(), original);
        assert!(original.source_key().starts_with("timer-config-recurring-"));
        let scoped = Recurrence::parse(&a, Some("one")).unwrap();
        assert_ne!(original.id, scoped.id);
        assert!(
            scoped
                .source_key()
                .starts_with("timer-invocation-recurring-")
        );
        assert_ne!(scoped.id, Recurrence::parse(&a, Some("two")).unwrap().id);
        a.name = "check-deploy 🦀".into();
        assert_ne!(original.id, Recurrence::parse(&a, None).unwrap().id);
        a.name = original.generation.name.clone();
        a.repeat_every = Some("2h".into());
        assert_ne!(original.id, Recurrence::parse(&a, None).unwrap().id);
        a.repeat_every = Some("1h".into());
        a.due_at = "1970-01-01T00:01:41Z".into();
        assert_ne!(original.id, Recurrence::parse(&a, None).unwrap().id);
    }

    #[test]
    fn rejects_zero_unsupported_and_overflowing_intervals() {
        let mut a = source();
        for raw in [
            "",
            "0",
            "0s",
            "0m",
            "0h",
            "-1s",
            "1.5s",
            "1ms",
            "1d",
            "abc",
            "18446744073709551615h",
            "18446744073709551615s",
        ] {
            a.repeat_every = Some(raw.into());
            assert!(Recurrence::parse(&a, None).is_err(), "{raw}");
            assert!(super::super::parse(&a).is_err(), "arm validation: {raw}");
        }
        a.repeat_every = None;
        assert!(super::super::parse(&a).is_ok());
        assert!(Recurrence::parse(&a, None).is_err());
        a.repeat_every = Some("1s".into());
        assert!(Recurrence::parse(&a, Some("")).is_err());
    }

    #[test]
    fn catches_up_decades_in_one_snapshot_and_preserves_phase_after_ack() {
        let r = Recurrence::parse(&source(), None).unwrap();
        assert_eq!(r.catch_up(None, 99).unwrap(), None);
        assert_eq!(r.next_due(None).unwrap(), 100);
        assert_eq!(
            r.catch_up(None, 100).unwrap(),
            Some(Slots {
                first: 0,
                last: 0,
                count: 1,
                due_at: 100
            })
        );
        let wall = 100 + 10 * 200_000_000 + 7;
        let catchup = r.catch_up(None, wall).unwrap().unwrap();
        assert_eq!(
            catchup,
            Slots {
                first: 0,
                last: 200_000_000,
                count: 200_000_001,
                due_at: wall - 7
            }
        );
        // More ticks while pending are covered by acknowledgement, not by
        // rewriting the snapshot or manufacturing intermediate occurrences.
        let covered = r.ack_coverage(None, catchup.last, wall + 100).unwrap();
        assert_eq!(covered, catchup.last + 10);
        assert_eq!(r.catch_up(Some(covered), wall + 100).unwrap(), None);
        assert_eq!(r.next_due(Some(covered)).unwrap(), wall + 103);
        let later = r.catch_up(Some(covered), wall + 1000).unwrap().unwrap();
        assert_eq!(later.first, covered + 1);
        assert_eq!(later.last, catchup.last + 100);
        assert_eq!(later.count, 90);
        assert_eq!(catchup.last, 200_000_000, "original snapshot is immutable");
        assert_ne!(
            r.producer_id(catchup.last).unwrap(),
            r.producer_id(later.last).unwrap()
        );
    }

    #[test]
    fn ack_exact_boundaries_and_backward_clocks_never_reopen_coverage() {
        let r = Recurrence::parse(&source(), None).unwrap();
        for (at, covered, next) in [
            (109, 0, 110),
            (110, 1, 120),
            (111, 1, 120),
            (139, 3, 140),
            (140, 4, 150),
        ] {
            assert_eq!(r.ack_coverage(None, 0, at).unwrap(), covered);
            assert_eq!(r.next_due(Some(covered)).unwrap(), next);
        }
        assert_eq!(r.ack_coverage(None, 5, 90).unwrap(), 5);
        assert_eq!(r.ack_coverage(Some(8), 5, 90).unwrap(), 8);
        assert_eq!(r.catch_up(Some(8), 140).unwrap(), None);
        assert_eq!(r.catch_up(Some(8), 190).unwrap().unwrap().first, 9);
    }

    #[test]
    fn impossible_persisted_slots_and_exhausted_next_deadlines_fail() {
        let r = Recurrence::parse(&source(), None).unwrap();
        assert!(r.due_at(u64::MAX).is_err());
        assert!(r.ack_coverage(None, u64::MAX, 100).is_err());
        assert!(r.catch_up(Some(u64::MAX), 100).is_err());
        let max = (u64::MAX - 100) / 10;
        assert!(r.next_due(Some(max)).is_err());
        let mut bad = r.generation.clone();
        bad.interval = 0;
        assert!(Recurrence::from_generation(bad).is_err());
        let mut bad = r.generation.clone();
        bad.anchor = i64::MAX as u64;
        assert!(Recurrence::from_generation(bad).is_err());
    }

    #[tokio::test]
    async fn recurrence_constructor_is_passive_until_the_owner_arms_it() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let specs = [(
            clank_core::agent_config::WaitEventSource::Timer(source()),
            super::super::SourceOrigin::Config,
        )];
        let result = super::super::TimerSet::new(&specs, dir.path(), None, tx.into());
        assert!(result.is_ok());
        assert!(rx.try_recv().is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
