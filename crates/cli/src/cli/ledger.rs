//! What the monitor owes each agent.
//!
//! One entry per item a fold admits for the agent, carrying two facts that
//! are never inferred from each other: its [`Obligation`], which is
//! [`super::wait::admitted_for`]'s decision with a time attached, and its
//! [`Delivery`] to the session that holds the label now. Recovery is owed
//! for the first; the second only says what is shown and which recovery
//! applies.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clank_core::wait::WaitItem;

use super::wait::Standing;

/// When an item became owed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Since {
    /// Unix seconds, from the item's own durable record, or from the
    /// answer or receipt file of the block that last withheld it.
    At(u64),
    /// No record dates it. `held` is when this ledger first held it: a
    /// bound on how recently it could have become owed, never its age.
    Unknown { held: u64 },
}

impl Since {
    pub(crate) fn known(self) -> Option<u64> {
        match self {
            Since::At(at) => Some(at),
            Since::Unknown { .. } => None,
        }
    }

    /// The latest moment it can have become owed.
    pub(crate) fn no_later_than(self) -> u64 {
        match self {
            Since::At(at) => at,
            Since::Unknown { held } => held,
        }
    }
}

/// Whether an item is owed, as the admission rule decided it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Obligation {
    /// Review-cycle work pending blocks pause, and those blocks, each by
    /// its [`crate::cli::block::key`].
    Withheld {
        by: Vec<String>,
    },
    Owed {
        owed_since: Since,
    },
    /// Acked, or its gate moved: the fold no longer admits it.
    Discharged,
}

/// When each block that no longer withholds anything from the agent
/// stopped doing so, by its key: [`crate::cli::block::ended`].
pub(crate) type Ended = BTreeMap<String, u64>;

/// Whether an owed item reached the session bound now. A receipt from any
/// other session is not evidence of either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "is", content = "expires", rename_all = "snake_case")]
pub(crate) enum Delivery {
    NotDelivered,
    /// Handed to a live carrier that has not answered, until `expires`
    /// (unix seconds).
    InFlight(u64),
    Delivered,
}

/// One item of a fold, as the ledger is told of it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Admission {
    pub(crate) identity: String,
    pub(crate) item: WaitItem,
    pub(crate) standing: Standing,
    /// A block withholds it while one is pending.
    pub(crate) pausable: bool,
    /// When the item's own durable record says it arrived, if it says.
    pub(crate) arrived: Option<u64>,
}

/// What proves delivery to the current binding.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Evidence {
    /// [`super::presentation::delivered_to`] the bound session.
    pub(crate) delivered: BTreeMap<String, u64>,
    /// The presentation in flight: what it carries, and when its carrier's
    /// silence becomes a failure (unix seconds).
    pub(crate) in_flight: Option<(Vec<String>, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) identity: String,
    pub(crate) kind: &'static str,
    pub(crate) what: String,
    pub(crate) obligation: Obligation,
    pub(crate) delivery: Delivery,
}

/// Everything one agent is owed or has withheld from it at a fold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Ledger {
    pub(crate) entries: Vec<Entry>,
    /// The newest receipt among the owed entries (unix seconds): the last
    /// time owed work reached the current binding.
    pub(crate) last_delivery: Option<u64>,
}

/// Each item's obligation, by identity: all of a ledger that a later fold
/// needs.
pub(crate) type Obligations = BTreeMap<String, Obligation>;

/// An owed entry: the only kind recovery acts for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Owed<'a> {
    pub(crate) entry: &'a Entry,
    pub(crate) owed_since: Since,
}

impl Ledger {
    /// The owed entries, the longest owed first.
    pub(crate) fn owed(&self) -> Vec<Owed<'_>> {
        let mut owed: Vec<Owed<'_>> = self
            .entries
            .iter()
            .filter_map(|entry| match entry.obligation {
                Obligation::Owed { owed_since } => Some(Owed { entry, owed_since }),
                Obligation::Withheld { .. } | Obligation::Discharged => None,
            })
            .collect();
        owed.sort_by_key(|o| o.owed_since.no_later_than());
        owed
    }

    /// When the delivery in flight, if any owed item is in one at `now`,
    /// stops counting as a delivery.
    pub(crate) fn in_flight_until(&self, now: u64) -> Option<u64> {
        self.owed()
            .iter()
            .filter_map(|o| match o.entry.delivery {
                Delivery::InFlight(expires) if now < expires => Some(expires),
                _ => None,
            })
            .max()
    }

    /// Since when the agent is known to have been owed something. `None`
    /// when no owed item is dated.
    pub(crate) fn owed_since(&self) -> Option<u64> {
        self.owed()
            .iter()
            .filter_map(|o| o.owed_since.known())
            .min()
    }

    pub(crate) fn obligations(&self) -> Obligations {
        self.entries
            .iter()
            .map(|e| (e.identity.clone(), e.obligation.clone()))
            .collect()
    }
}

/// The obligation an [`Admission`] has, given the one it had `before`.
///
/// Time withheld is never charged, and the boundary is the block's own
/// record, not when a monitor looked: work a block released is owed from
/// the moment the last of its blocks ended, as `ended` dates them. Work
/// that was never seen withheld is owed from its arrival, or from a block
/// that ended after it arrived, since that block withheld it until then.
///
/// Where the boundary cannot be dated (an arrival no record gives, a
/// block cleaned away before its end was read) the item is owed since an
/// unknown moment, never since `now`.
pub(crate) fn obligation(
    before: Option<&Obligation>,
    admitted: Option<&Admission>,
    ended: &Ended,
    now: u64,
) -> Obligation {
    let Some(a) = admitted else {
        return Obligation::Discharged;
    };
    let undated = Since::Unknown { held: now };
    let released = |by: &[String]| {
        by.iter()
            .map(|block| ended.get(block).copied())
            .collect::<Option<Vec<u64>>>()
            .and_then(|ends| ends.into_iter().max())
    };
    let owed_since = match (&a.standing, before) {
        (Standing::Context, _) => return Obligation::Discharged,
        (Standing::Withheld { by }, _) => return Obligation::Withheld { by: by.clone() },
        (Standing::Owed, Some(Obligation::Withheld { by })) => match released(by) {
            Some(end) => Since::At(end.max(a.arrived.unwrap_or(0))),
            None => undated,
        },
        (Standing::Owed, Some(Obligation::Owed { owed_since })) => match (owed_since, a.arrived) {
            (Since::Unknown { .. }, Some(at)) if !a.pausable => Since::At(at),
            // A block asked and ended between two folds withheld it all
            // the same.
            (Since::At(at), _) if a.pausable => {
                Since::At(ended.values().copied().fold(*at, u64::max))
            }
            (since, _) => *since,
        },
        (Standing::Owed, None | Some(Obligation::Discharged)) => match a.arrived {
            Some(at) if a.pausable => Since::At(ended.values().copied().fold(at, u64::max)),
            Some(at) => Since::At(at),
            None => undated,
        },
    };
    Obligation::Owed { owed_since }
}

/// Whether `identity` reached the current binding, by `evidence` alone. A
/// delivery in flight past its expiry is no delivery.
pub(crate) fn delivery(identity: &str, evidence: &Evidence, now: u64) -> Delivery {
    if evidence.delivered.contains_key(identity) {
        return Delivery::Delivered;
    }
    match &evidence.in_flight {
        Some((carried, expires)) if now < *expires && carried.iter().any(|i| i == identity) => {
            Delivery::InFlight(*expires)
        }
        _ => Delivery::NotDelivered,
    }
}

/// The ledger after a fold that admitted `admitted`, given the one
/// `before` it. An item the fold no longer admits is discharged, and gone.
pub(crate) fn fold(
    before: &Obligations,
    admitted: &[Admission],
    evidence: &Evidence,
    ended: &Ended,
    now: u64,
) -> Ledger {
    let entries: Vec<Entry> = admitted
        .iter()
        .filter_map(|a| {
            let obligation = obligation(before.get(&a.identity), Some(a), ended, now);
            (obligation != Obligation::Discharged).then(|| Entry {
                identity: a.identity.clone(),
                kind: kind(&a.item),
                what: what(&a.item).unwrap_or_else(|| "a notice".to_string()),
                delivery: delivery(&a.identity, evidence, now),
                obligation,
            })
        })
        .collect();
    let last_delivery = entries
        .iter()
        .filter(|e| matches!(e.obligation, Obligation::Owed { .. }))
        .filter_map(|e| evidence.delivered.get(&e.identity).copied())
        .max();
    Ledger {
        entries,
        last_delivery,
    }
}

/// What kind of thing an item is, as `clank status -j` names it.
pub(crate) fn kind(item: &WaitItem) -> &'static str {
    match item {
        WaitItem::Message { .. } => "message",
        WaitItem::GithubEvent { .. } => "github_event",
        WaitItem::TimerEvent { .. } => "timer",
        WaitItem::CommandEvent { .. } => "command_event",
        WaitItem::Reviewer { .. } | WaitItem::AdHocReview { .. } => "review",
        WaitItem::AdHocRevise { .. } => "revision",
        WaitItem::Master { .. } => "work",
        WaitItem::FixCommitTag { .. } => "tag_fix",
        WaitItem::PromoteFromQueue { .. } => "promotion",
        WaitItem::Unblocked { .. } => "answer",
        WaitItem::Idle { .. }
        | WaitItem::ForCommit { .. }
        | WaitItem::ForFinished { .. }
        | WaitItem::ForBlocked { .. }
        | WaitItem::Blocked { .. } => "notice",
    }
}

/// What an item is, in the words a roster row says an agent owes it in.
/// `None` for a notice, which names no work.
pub(crate) fn what(item: &WaitItem) -> Option<String> {
    let short = |sha: &clank_core::ids::CommitSha| {
        let sha = sha.as_str();
        sha[..7.min(sha.len())].to_string()
    };
    Some(match item {
        WaitItem::Reviewer { sha, .. } => format!("review of {}", short(sha)),
        WaitItem::AdHocReview { sha, .. } => format!("review of {} (ad-hoc)", short(sha)),
        WaitItem::AdHocRevise { sha } => format!("revision of {} (ad-hoc)", short(sha)),
        WaitItem::Master { plan, .. } => format!("work on {}", plan.as_str()),
        WaitItem::FixCommitTag { sha, .. } => format!("tag fix on {}", short(sha)),
        WaitItem::PromoteFromQueue { name, .. } => format!("promotion of {name}"),
        WaitItem::Message { from, .. } => match from {
            Some(from) => format!("message from {from}"),
            None => "message".to_string(),
        },
        WaitItem::GithubEvent { event, number, .. } => match number {
            Some(n) => format!("github {event} #{n}"),
            None => format!("github {event}"),
        },
        WaitItem::TimerEvent { name, .. } => format!("timer {name}"),
        WaitItem::CommandEvent { name, .. } => format!("event from {name}"),
        WaitItem::Unblocked { carries, .. } => format!("answer to {}", carries.name),
        WaitItem::Idle { .. }
        | WaitItem::ForCommit { .. }
        | WaitItem::ForFinished { .. }
        | WaitItem::ForBlocked { .. }
        | WaitItem::Blocked { .. } => return None,
    })
}

/// One stored obligation. Only obligations outlive the monitor, so that a
/// restart neither restarts an owed item's clock nor forgets that one was
/// withheld; what an item is and whether it was delivered are read afresh.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Stored {
    identity: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "blocks"
    )]
    withheld: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owed_since: Option<Since>,
}

/// The withholding blocks, which a ledger stored before each was named
/// apart kept as one line: `claude/a, codex/b (answered, not yet
/// received)`.
fn blocks<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Shape {
        Each(Vec<String>),
        Line(String),
    }
    Ok(
        <Option<Shape> as serde::Deserialize>::deserialize(d)?.map(|shape| match shape {
            Shape::Each(blocks) => blocks,
            Shape::Line(line) => line
                .replace(" (answered, not yet received)", "")
                .split(", ")
                .map(str::to_string)
                .collect(),
        }),
    )
}

pub(crate) fn path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("ledger.json")
}

/// The obligations last stored in `agent_dir`; none when there is no
/// ledger there or it cannot be read.
pub(crate) fn load(agent_dir: &Path) -> Obligations {
    std::fs::read(path(agent_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<Stored>>(&bytes).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|s| {
            let obligation = match (s.withheld, s.owed_since) {
                (Some(by), _) => Obligation::Withheld { by },
                (None, Some(owed_since)) => Obligation::Owed { owed_since },
                (None, None) => return None,
            };
            Some((s.identity, obligation))
        })
        .collect()
}

/// Store `obligations`, or remove the file when there are none.
pub(crate) fn store(agent_dir: &Path, obligations: &Obligations) -> std::io::Result<()> {
    let _permit = crate::upgrade::writing()?;
    let path = path(agent_dir);
    if obligations.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let stored: Vec<Stored> = obligations
        .iter()
        .map(|(identity, obligation)| Stored {
            identity: identity.clone(),
            withheld: match obligation {
                Obligation::Withheld { by } => Some(by.clone()),
                _ => None,
            },
            owed_since: match obligation {
                Obligation::Owed { owed_since } => Some(*owed_since),
                _ => None,
            },
        })
        .collect();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&stored)?)?;
    std::fs::rename(&tmp, &path)
}

/// [`fold`] against the obligations stored in `agent_dir`, stored again
/// when one changed.
pub(crate) fn fold_stored(
    agent_dir: &Path,
    admitted: &[Admission],
    evidence: &Evidence,
    ended: &Ended,
    now: u64,
) -> Ledger {
    let before = load(agent_dir);
    let after = fold(&before, admitted, evidence, ended, now);
    let obligations = after.obligations();
    if obligations != before
        && let Err(e) = store(agent_dir, &obligations)
    {
        eprintln!(
            "monitor: could not record what {} is owed: {e}",
            agent_dir.display()
        );
    }
    after
}

#[cfg(test)]
impl Ledger {
    /// One item owed since unix second 0 and never delivered when `owed`,
    /// else nothing.
    pub(crate) fn owing_if(owed: bool) -> Self {
        Ledger {
            entries: owed
                .then(|| Entry {
                    identity: "continue plan=p".into(),
                    kind: "work",
                    what: "work on p".into(),
                    obligation: Obligation::Owed {
                        owed_since: Since::At(0),
                    },
                    delivery: Delivery::NotDelivered,
                })
                .into_iter()
                .collect(),
            last_delivery: None,
        }
    }
}

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;
