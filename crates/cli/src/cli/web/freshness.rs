//! When a viewer's Terminal may read as live: which `verify` requests a
//! source sample may answer, when the next sample may start, and what a
//! sample's result does.
//!
//! Pure, over injected instants. The bridge owns the sampling child and
//! the retained panes; this decides. A sample answers only requests that
//! reached the bridge before it was acquired, so a confirmation is never
//! older than the request it answers, whatever the transit delay.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

/// Regular samples start at least this far apart.
pub(crate) const SPACING: Duration = Duration::from_secs(15);
/// A mismatch is confirmed this long after the sample that found it.
pub(crate) const CONFIRM_AFTER: Duration = Duration::from_secs(2);
/// A connection's demand lapses this long after its last `verify`.
pub(crate) const DEMAND_TTL: Duration = Duration::from_secs(30);
/// A sample still running this long after it started is abandoned.
pub(crate) const SAMPLE_TIMEOUT: Duration = Duration::from_secs(5);

/// A viewer's request, as the host routed it: the browser connection
/// and the id the browser timed it by.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct RequestId {
    pub(crate) tab: String,
    pub(crate) id: String,
}

/// The retained state a sample is ordered against, per pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Retained {
    pub(crate) generation: u64,
    pub(crate) revisions: BTreeMap<String, u64>,
    pub(crate) viewports: BTreeMap<String, Vec<String>>,
    /// Panes open now, with or without a viewport yet: a stream that
    /// never delivered its first frame can still be sampled.
    pub(crate) live: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Regular,
    Confirm,
}

#[derive(Debug, Clone)]
struct Sample {
    kind: Kind,
    started: Instant,
    generation: u64,
    revisions: BTreeMap<String, u64>,
}

#[derive(Debug, Clone)]
struct Waiting {
    request: RequestId,
    panes: BTreeSet<String>,
    arrived: Instant,
}

/// Start a sample of these panes now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Start {
    pub(crate) panes: BTreeSet<String>,
}

/// What a finished sample means.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Outcome {
    /// `confirmed { id, pane, revision }`, per answered request.
    pub(crate) confirmed: Vec<(RequestId, String, u64)>,
    /// Panes whose subscription is proven stalled: publish this content
    /// as a new revision and restart the subscription.
    pub(crate) stalled: BTreeMap<String, Vec<String>>,
}

#[derive(Default)]
pub(crate) struct Freshness {
    demand: HashMap<String, (BTreeSet<String>, Instant)>,
    waiting: Vec<Waiting>,
    in_flight: Option<Sample>,
    last_regular: Option<Instant>,
    /// A mismatch awaiting its confirming sample, captured at the
    /// regular sample's start.
    confirm: Option<(Instant, u64, BTreeMap<String, u64>)>,
}

impl Freshness {
    /// A connection's `verify`. An empty set ends its demand.
    pub(crate) fn verify(&mut self, request: RequestId, panes: BTreeSet<String>, now: Instant) {
        if panes.is_empty() {
            self.demand.remove(&request.tab);
            self.waiting.retain(|w| w.request.tab != request.tab);
            return;
        }
        self.demand
            .insert(request.tab.clone(), (panes.clone(), now));
        self.waiting.push(Waiting {
            request,
            panes,
            arrived: now,
        });
    }

    fn expire(&mut self, now: Instant) {
        self.demand
            .retain(|_, (_, at)| now.duration_since(*at) < DEMAND_TTL);
        self.waiting
            .retain(|w| now.duration_since(w.arrived) < DEMAND_TTL);
    }

    fn demanded(&self) -> BTreeSet<String> {
        self.demand
            .values()
            .flat_map(|(panes, _)| panes.iter().cloned())
            .collect()
    }

    /// Whether to start a sample now, and of what. At most one runs, and
    /// only for panes someone still demands that are open now.
    pub(crate) fn tick(&mut self, now: Instant, retained: &Retained) -> Option<Start> {
        self.expire(now);
        if self.in_flight.is_some() {
            return None;
        }
        let wanted: BTreeSet<String> = self
            .demanded()
            .into_iter()
            .filter(|p| retained.live.contains(p))
            .collect();
        let (kind, panes) = match self.confirm.take() {
            // The one bounded exception to the spacing: it is consumed
            // here, and covers only panes still demanded.
            Some((at, generation, captured)) if now >= at => {
                let panes: BTreeSet<String> = captured
                    .keys()
                    .filter(|p| wanted.contains(*p))
                    .cloned()
                    .collect();
                if panes.is_empty() {
                    return None;
                }
                self.confirm = Some((at, generation, captured));
                (Kind::Confirm, panes)
            }
            Some(pending) => {
                self.confirm = Some(pending);
                return None;
            }
            None => {
                let spaced = self
                    .last_regular
                    .is_none_or(|last| now.duration_since(last) >= SPACING);
                // A waiting request no completed sample can answer: every
                // one still waiting arrived after the last sample started.
                if self.waiting.is_empty() || !spaced || wanted.is_empty() {
                    return None;
                }
                (Kind::Regular, wanted)
            }
        };
        if kind == Kind::Regular {
            self.last_regular = Some(now);
        }
        self.in_flight = Some(Sample {
            kind,
            started: now,
            generation: retained.generation,
            revisions: panes
                .iter()
                .map(|p| (p.clone(), retained.revisions.get(p).copied().unwrap_or(0)))
                .collect(),
        });
        Some(Start { panes })
    }

    /// The running sample could not be taken. Its waiters wait on, for
    /// the next regular sample; a failed confirmation is not retried.
    pub(crate) fn abandon(&mut self) {
        if let Some(sample) = self.in_flight.take()
            && sample.kind == Kind::Confirm
        {
            self.confirm = None;
        }
    }

    /// The running sample took too long: give it up, as [`Self::abandon`].
    pub(crate) fn timed_out(&mut self, now: Instant) -> bool {
        let over = self
            .in_flight
            .as_ref()
            .is_some_and(|s| now.duration_since(s.started) >= SAMPLE_TIMEOUT);
        if over {
            self.abandon();
        }
        over
    }

    /// The running sample finished with these viewports.
    pub(crate) fn done(
        &mut self,
        sampled: BTreeMap<String, Vec<String>>,
        retained: &Retained,
        now: Instant,
    ) -> Outcome {
        let Some(sample) = self.in_flight.take() else {
            return Outcome::default();
        };
        let mut outcome = Outcome::default();
        let mut matched: BTreeMap<String, u64> = BTreeMap::new();
        let mut differing: BTreeMap<String, u64> = BTreeMap::new();
        if sample.generation == retained.generation {
            for (pane, rev) in &sample.revisions {
                let current = retained.revisions.get(pane).copied().unwrap_or(0) == *rev;
                let (Some(seen), true) = (sampled.get(pane), current) else {
                    continue;
                };
                // No viewport yet differs from any sample: a stream that
                // never delivered its first frame is repaired like a stall.
                if retained.viewports.get(pane) == Some(seen) {
                    matched.insert(pane.clone(), *rev);
                } else {
                    differing.insert(pane.clone(), *rev);
                }
            }
        }
        match sample.kind {
            Kind::Regular => {
                if !differing.is_empty() {
                    self.confirm = Some((now + CONFIRM_AFTER, sample.generation, differing));
                }
            }
            Kind::Confirm => {
                let (generation, captured) = match self.confirm.take() {
                    Some((_, generation, captured)) => (generation, captured),
                    None => (0, BTreeMap::new()),
                };
                for (pane, rev) in differing {
                    if generation == sample.generation
                        && captured.get(&pane) == Some(&rev)
                        && let Some(seen) = sampled.get(&pane)
                    {
                        outcome.stalled.insert(pane.clone(), seen.clone());
                    }
                }
            }
        }
        // Answer only requests that reached the bridge before this
        // sample was acquired.
        let answered: Vec<Waiting> = {
            let (before, after): (Vec<_>, Vec<_>) = std::mem::take(&mut self.waiting)
                .into_iter()
                .partition(|w| w.arrived <= sample.started);
            self.waiting = after;
            before
        };
        let mut still: Vec<Waiting> = Vec::new();
        for w in answered {
            let mut open = BTreeSet::new();
            for pane in &w.panes {
                if let Some(rev) = matched.get(pane) {
                    outcome
                        .confirmed
                        .push((w.request.clone(), pane.clone(), *rev));
                } else if self.confirm.is_some() || !outcome.stalled.is_empty() {
                    open.insert(pane.clone());
                }
            }
            for pane in outcome.stalled.keys() {
                if w.panes.contains(pane) {
                    let rev = retained.revisions.get(pane).copied().unwrap_or(0) + 1;
                    outcome
                        .confirmed
                        .push((w.request.clone(), pane.clone(), rev));
                    open.remove(pane);
                }
            }
            if !open.is_empty() && self.confirm.is_some() {
                still.push(Waiting { panes: open, ..w });
            }
        }
        self.waiting.extend(still);
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panes(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn retained(rev: u64, screen: &str) -> Retained {
        Retained {
            generation: 1,
            revisions: [("p".to_string(), rev)].into(),
            viewports: [("p".to_string(), vec![screen.to_string()])].into(),
            live: ["p".to_string()].into(),
        }
    }

    fn req(tab: u64, id: &str) -> RequestId {
        RequestId {
            tab: tab.to_string(),
            id: id.into(),
        }
    }

    fn seen(screen: &str) -> BTreeMap<String, Vec<String>> {
        [("p".to_string(), vec![screen.to_string()])].into()
    }

    #[test]
    fn nothing_samples_without_demand() {
        let mut f = Freshness::default();
        let t = Instant::now();
        assert_eq!(f.tick(t, &retained(1, "a")), None);
        f.verify(req(1, "r"), panes(&["p"]), t);
        f.verify(req(1, "r2"), BTreeSet::new(), t);
        assert_eq!(
            f.tick(t, &retained(1, "a")),
            None,
            "an empty verify ends demand"
        );
    }

    #[test]
    fn a_matching_sample_confirms_the_request_it_postdates() {
        let mut f = Freshness::default();
        let t = Instant::now();
        let r = retained(4, "a");
        f.verify(req(1, "r"), panes(&["p"]), t);
        assert_eq!(
            f.tick(t, &r),
            Some(Start {
                panes: panes(&["p"])
            })
        );
        let out = f.done(seen("a"), &r, t + Duration::from_millis(50));
        assert_eq!(out.confirmed, vec![(req(1, "r"), "p".to_string(), 4)]);
        assert!(out.stalled.is_empty());
    }

    #[test]
    fn a_request_arriving_during_a_sample_waits_for_the_next() {
        let mut f = Freshness::default();
        let t = Instant::now();
        let r = retained(1, "a");
        f.verify(req(1, "a"), panes(&["p"]), t);
        f.tick(t, &r).unwrap();
        let late = t + Duration::from_millis(10);
        f.verify(req(2, "b"), panes(&["p"]), late);
        let out = f.done(seen("a"), &r, t + Duration::from_millis(50));
        assert_eq!(
            out.confirmed,
            vec![(req(1, "a"), "p".to_string(), 1)],
            "B borrows nothing"
        );
        assert_eq!(
            f.tick(t + Duration::from_secs(1), &r),
            None,
            "spacing holds"
        );
        let next = t + SPACING;
        assert!(f.tick(next, &r).is_some(), "B gets the next shared sample");
        let out = f.done(seen("a"), &r, next + Duration::from_millis(50));
        assert!(out.confirmed.contains(&(req(2, "b"), "p".to_string(), 1)));
    }

    #[test]
    fn a_sample_overtaken_by_a_newer_frame_confirms_nothing() {
        let mut f = Freshness::default();
        let t = Instant::now();
        f.verify(req(1, "r"), panes(&["p"]), t);
        f.tick(t, &retained(1, "a")).unwrap();
        let out = f.done(seen("a"), &retained(2, "b"), t + Duration::from_millis(50));
        assert!(out.confirmed.is_empty());
        assert!(out.stalled.is_empty());
    }

    #[test]
    fn a_changing_healthy_pane_is_not_declared_stalled() {
        let mut f = Freshness::default();
        let t = Instant::now();
        f.verify(req(1, "r"), panes(&["p"]), t);
        f.tick(t, &retained(1, "a")).unwrap();
        let out = f.done(seen("b"), &retained(1, "a"), t);
        assert!(out.confirmed.is_empty() && out.stalled.is_empty());
        let at = t + CONFIRM_AFTER;
        // The subscription delivered "b" meanwhile: revision 2.
        f.tick(at, &retained(2, "b")).unwrap();
        let out = f.done(seen("b"), &retained(2, "b"), at);
        assert!(out.stalled.is_empty(), "the frame moved the revision");
    }

    #[test]
    fn two_differing_samples_prove_a_stall_and_answer_the_waiters() {
        let mut f = Freshness::default();
        let t = Instant::now();
        let r = retained(1, "a");
        f.verify(req(1, "r"), panes(&["p"]), t);
        f.tick(t, &r).unwrap();
        f.done(seen("b"), &r, t);
        assert_eq!(
            f.tick(t + Duration::from_millis(500), &r),
            None,
            "not before CONFIRM_AFTER"
        );
        let at = t + CONFIRM_AFTER;
        assert!(
            f.tick(at, &r).is_some(),
            "the one bounded exception to spacing"
        );
        let out = f.done(seen("c"), &r, at);
        assert_eq!(
            out.stalled,
            [("p".to_string(), vec!["c".to_string()])].into()
        );
        assert_eq!(out.confirmed, vec![(req(1, "r"), "p".to_string(), 2)]);
    }

    #[test]
    fn a_hung_sample_is_abandoned_and_its_waiters_wait_on() {
        let mut f = Freshness::default();
        let t = Instant::now();
        let r = retained(1, "a");
        f.verify(req(1, "r"), panes(&["p"]), t);
        f.tick(t, &r).unwrap();
        assert!(!f.timed_out(t + Duration::from_secs(1)));
        assert!(f.timed_out(t + SAMPLE_TIMEOUT));
        let next = t + SPACING;
        assert!(f.tick(next, &r).is_some(), "single flight is free again");
    }

    /// Demand alone does not sample: an answered request leaves nothing
    /// waiting until the viewer asks again.
    #[test]
    fn an_answered_request_is_not_sampled_again_until_the_next() {
        let mut f = Freshness::default();
        let t = Instant::now();
        let r = retained(1, "a");
        f.verify(req(1, "r"), panes(&["p"]), t);
        f.tick(t, &r).unwrap();
        f.done(seen("a"), &r, t);
        assert_eq!(
            f.tick(t + SPACING, &r),
            None,
            "demand unexpired, nothing waiting"
        );
        f.verify(req(1, "r2"), panes(&["p"]), t + SPACING);
        assert!(f.tick(t + SPACING, &r).is_some());
    }

    /// Proves a mismatch, ready for its confirming sample.
    fn mismatched(f: &mut Freshness, t: Instant, r: &Retained) {
        f.verify(req(1, "r"), panes(&["p"]), t);
        f.tick(t, r).unwrap();
        f.done(seen("b"), r, t);
    }

    #[test]
    fn withdrawn_demand_cancels_the_confirming_sample() {
        let mut f = Freshness::default();
        let t = Instant::now();
        let r = retained(1, "a");
        mismatched(&mut f, t, &r);
        f.verify(req(1, "off"), BTreeSet::new(), t + Duration::from_secs(1));
        assert_eq!(f.tick(t + CONFIRM_AFTER, &r), None);
    }

    #[test]
    fn expired_demand_cancels_the_confirming_sample() {
        let mut f = Freshness::default();
        let t = Instant::now();
        let r = retained(1, "a");
        f.verify(req(1, "r"), panes(&["p"]), t);
        let late = t + Duration::from_secs(29);
        f.tick(late, &r).unwrap();
        f.done(seen("b"), &r, late);
        assert_eq!(
            f.tick(late + CONFIRM_AFTER, &r),
            None,
            "demand expired at 30 s"
        );
    }

    #[test]
    fn a_failed_confirming_sample_is_not_retried_outside_the_budget() {
        let mut f = Freshness::default();
        let t = Instant::now();
        let r = retained(1, "a");
        mismatched(&mut f, t, &r);
        let at = t + CONFIRM_AFTER;
        assert!(f.tick(at, &r).is_some());
        f.abandon();
        f.verify(req(1, "r2"), panes(&["p"]), at);
        assert_eq!(
            f.tick(at + Duration::from_secs(1), &r),
            None,
            "no second confirmation"
        );
        assert!(
            f.tick(t + SPACING, &r).is_some(),
            "back to the regular budget"
        );
        let mut g = Freshness::default();
        mismatched(&mut g, t, &r);
        assert!(g.tick(at, &r).is_some());
        assert!(g.timed_out(at + SAMPLE_TIMEOUT));
        assert_eq!(
            g.tick(at + SAMPLE_TIMEOUT, &r),
            None,
            "a timeout consumes it too"
        );
    }

    #[test]
    fn a_pane_without_a_first_frame_is_sampled_and_repaired() {
        let mut f = Freshness::default();
        let t = Instant::now();
        let r = Retained {
            generation: 1,
            revisions: BTreeMap::new(),
            viewports: BTreeMap::new(),
            live: ["p".to_string()].into(),
        };
        f.verify(req(1, "r"), panes(&["p"]), t);
        assert!(f.tick(t, &r).is_some(), "open but silent is still sampled");
        f.done(seen("a"), &r, t);
        let at = t + CONFIRM_AFTER;
        assert!(f.tick(at, &r).is_some());
        let out = f.done(seen("a"), &r, at);
        assert_eq!(
            out.stalled,
            [("p".to_string(), vec!["a".to_string()])].into()
        );
    }

    #[test]
    fn demand_lapses_without_renewal() {
        let mut f = Freshness::default();
        let t = Instant::now();
        let r = retained(1, "a");
        f.verify(req(1, "r"), panes(&["p"]), t);
        f.tick(t, &r).unwrap();
        f.done(seen("a"), &r, t);
        assert_eq!(f.tick(t + DEMAND_TTL + SPACING, &r), None);
    }

    #[test]
    fn two_viewers_share_one_sample_and_get_their_own_ids() {
        let mut f = Freshness::default();
        let t = Instant::now();
        let r = retained(3, "a");
        f.verify(req(1, "x"), panes(&["p"]), t);
        f.verify(req(2, "y"), panes(&["p"]), t);
        assert!(f.tick(t, &r).is_some());
        assert_eq!(f.tick(t, &r), None, "one in flight");
        let out = f.done(seen("a"), &r, t);
        assert_eq!(
            out.confirmed,
            vec![
                (req(1, "x"), "p".to_string(), 3),
                (req(2, "y"), "p".to_string(), 3)
            ]
        );
    }
}
