//! A declared subagent, through the work it may have started.
//!
//! A subagent that ends its turn to wait on its own background work leaves
//! the harness's listing and returns under the same id when that work
//! exits, so absence alone does not say it finished. What its session's
//! hooks report is reduced to a [`Sight`] of it, and [`transition`] is the
//! whole rule for what each one does.

use serde::{Deserialize, Deserializer, Serialize};

use super::Ms;

/// A listed task a stopped subagent may be waiting on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldTask {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Where a declared subagent stands by its session's reports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Running.
    #[default]
    Live,
    /// Stopped, and `on` may be work it started. `sure` when the evidence
    /// excludes the agent itself as that work's owner.
    Parked { on: Vec<HeldTask>, sure: bool },
    /// A report that could not be ordered says it may be running again:
    /// nothing about its tasks can end the attendance from here. Only a
    /// report whose hook started strictly after `since` moves it on, and
    /// with no `since` none can: the uncertain resume came with a missing
    /// time, or with one earlier than a report already applied, which is a
    /// clock that went back or a report from before it, and either way no
    /// later time proves a report came after that resume.
    Unsettled { since: Option<Ms> },
    /// The attendance is over. Never kept: the entry leaves with it, which
    /// releases what it held.
    Ended,
}

impl<'de> Deserialize<'de> for Phase {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Current {
            Live,
            Parked { on: Vec<HeldTask>, sure: bool },
            Unsettled { since: Option<Ms> },
            Ended,
        }
        /// The build that first kept phases wrote `"unsettled"` with no
        /// barrier: nothing it held proves a later report followed it.
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Bare {
            Unsettled,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Stored {
            Current(Current),
            Bare(Bare),
        }
        Ok(match Stored::deserialize(d)? {
            Stored::Current(Current::Live) => Phase::Live,
            Stored::Current(Current::Parked { on, sure }) => Phase::Parked { on, sure },
            Stored::Current(Current::Unsettled { since }) => Phase::Unsettled { since },
            Stored::Current(Current::Ended) => Phase::Ended,
            Stored::Bare(Bare::Unsettled) => Phase::Unsettled { since: None },
        })
    }
}

/// Whether a report's hook started after the reference it is judged
/// against. Arriving later is not being later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Later,
    /// At the same time, earlier, or with no time at all. `barrier` is
    /// the time a report must then be strictly after to be later than
    /// this one: the reference itself when the two times are equal, and
    /// none when the report's time is missing or earlier, since then no
    /// time bounds when it happened.
    Unordered {
        barrier: Option<Ms>,
    },
}

impl Order {
    pub fn of(report: Option<Ms>, reference: Option<Ms>) -> Self {
        match (report, reference) {
            (Some(report), Some(reference)) if report > reference => Self::Later,
            (Some(report), Some(reference)) if report == reference => Self::Unordered {
                barrier: Some(reference),
            },
            (Some(_), None) => Self::Later,
            _ => Self::Unordered { barrier: None },
        }
    }
}

/// What one report shows of a bound subagent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sight {
    /// Its start hook fired: spawned or resumed.
    Started,
    /// A listing other than its own stop's shows it.
    Listed,
    /// Its own stop. The listing still shows it as running: the report is
    /// the evidence that it stopped.
    Stopped {
        /// What that listing holds that it may have started.
        candidates: Vec<HeldTask>,
        sure: bool,
    },
    /// A listing without it.
    Absent {
        /// The listing is the agent's own Stop's, not another child's.
        agent_stop: bool,
        /// Every id the listing holds.
        live: Vec<String>,
        /// What it holds that the subagent may have started.
        candidates: Vec<HeldTask>,
    },
}

/// The transition table.
///
/// An unordered report that could only park or end the subagent changes
/// nothing; one that might be a resume unsettles a parked one. So every
/// unordered case errs toward an attendance that outlasts its work.
///
/// An unsettled subagent is judged against its own barrier, not the last
/// report applied: a report that cannot be shown to follow the uncertain
/// resume keeps it unsettled. Only a sight that could itself be a resume
/// (a start, or a listing showing it) weakens the barrier: equal keeps it,
/// missing or earlier removes it. An unordered stop or listing without it
/// is no resume, and leaves the barrier as it was.
pub fn transition(phase: Phase, order: Order, sight: Sight) -> Phase {
    use Order::{Later, Unordered};
    use Phase::{Ended, Live, Parked, Unsettled};
    let parked = |on: Vec<HeldTask>, sure| match on.is_empty() {
        true => Ended,
        false => Parked { on, sure },
    };
    match (phase, order, sight) {
        (Ended, _, _) => Ended,
        (_, Later, Sight::Started | Sight::Listed) => Live,
        (Parked { .. }, Unordered { barrier }, Sight::Started | Sight::Listed) => {
            Unsettled { since: barrier }
        }
        (Unsettled { .. }, Unordered { barrier }, Sight::Started | Sight::Listed) => {
            Unsettled { since: barrier }
        }
        (_, Later, Sight::Stopped { candidates, sure }) => parked(candidates, sure),
        (
            Live | Unsettled { .. },
            Later,
            Sight::Absent {
                agent_stop: true,
                candidates,
                ..
            },
        ) => parked(candidates, false),
        (Parked { on, sure }, Later, Sight::Absent { live, .. }) => parked(
            on.into_iter().filter(|t| live.contains(&t.id)).collect(),
            sure,
        ),
        (phase, _, _) => phase,
    }
}

/// A bound declaration's state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tracked {
    /// The ids seen before the listing that first showed the subagent: a
    /// baseline task is never its own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub baseline: Vec<String>,
    /// The hook time of the last report applied. An unsettled subagent is
    /// judged by its own barrier instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<Ms>,
    #[serde(default)]
    pub phase: Phase,
}

impl Tracked {
    pub(super) fn is_unobserved(&self) -> bool {
        *self == Self::default()
    }

    /// Where a report at `at` stands against what is already known.
    pub fn order(&self, at: Option<Ms>) -> Order {
        match self.phase {
            Phase::Unsettled { since: None } => Order::Unordered { barrier: None },
            Phase::Unsettled { since } => Order::of(at, since),
            _ => Order::of(at, self.as_of),
        }
    }

    pub(super) fn apply(&mut self, at: Option<Ms>, sight: Sight) {
        let order = self.order(at);
        self.phase = transition(std::mem::take(&mut self.phase), order, sight);
        if order == Order::Later {
            self.as_of = at;
        }
    }
}
