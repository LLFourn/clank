//! The scrollable-timeline model: the [`Seg`]uence of historical log
//! rows, the live [`InProgress`] activity synthesized from the active
//! plan's `WaitingOn` (rendered on the AGENTS panel rows, not in the
//! log), and the cursor→viewport math. Pure (no rendering, no IO) —
//! the renderer turns each [`Seg`] into spans, and the loop drives the
//! cursor.

use crate::cli::status::StatusSnapshot;
use clank_core::plan_view::WaitingOn;

/// A live, in-flight activity item synthesized from the active plan's
/// `WaitingOn` (NOT from `log_rows`, which is historical). Rendered on
/// the matching AGENTS panel row — activity lives where the actors are;
/// the log below is pure history. Carries the only animated glyph on
/// screen.
pub(super) enum InProgress {
    /// A registered reviewer who still owes a verdict on the latest
    /// reviewable commit — their panel row spins. `verb` is the italic
    /// wait-text ("reviewing" / "gate-reviewing").
    PendingReview {
        label: String,
        verb: &'static str,
        since: Option<i64>,
    },
    /// Master is producing the next commit — master's panel row spins.
    /// `verb` says which (working/revising/drafting/finalizing) via
    /// [`verb_of`].
    MasterWorking {
        name: String,
        verb: &'static str,
        since: Option<i64>,
    },
}

impl InProgress {
    /// When this agent's oldest outstanding obligation began, if any
    /// of them is dated.
    pub(super) fn since(&self) -> Option<i64> {
        match self {
            InProgress::PendingReview { since, .. } | InProgress::MasterWorking { since, .. } => {
                *since
            }
        }
    }
}

/// When the agent's OLDEST current obligation was handed to it.
///
/// The obligations are not re-derived: `work_for` is the same source
/// that decided this row spins at all, and each item is dated by the
/// stage boundary of the work state it names — never by "the newest
/// verdict", which a still-pending peer's verdict would move.
///
/// Read through `work_projection`, whose dropped multi-plan preempt
/// is what keeps master's per-plan obligations visible here: the
/// preempt is a routing decision with no stage of its own, and the
/// question this answers is how long the WORK has been waiting.
fn oldest_obligation(
    snap: &StatusSnapshot,
    work: &clank_core::wait::WorkStatus,
    label: &str,
    role: clank_core::vocab::Role,
) -> Option<i64> {
    use clank_core::wait::{HandoverTier, WaitItem};
    let Ok(parsed) = clank_core::ids::AgentLabel::parse(label) else {
        return None;
    };
    // The tier an agent answers on decides WHICH stage boundary
    // handed it its work: the commit tier is summoned by the commit
    // itself, everyone else by a tier closing.
    let tier = match snap
        .agents
        .iter()
        .find(|a| a.label == label)
        .map(|a| a.role)
    {
        Some(crate::cli::teams_config::RosterRole::Master) => HandoverTier::Master,
        Some(r) if r.in_commit_tier() => HandoverTier::Commit,
        Some(_) => HandoverTier::Second,
        // Off the roster entirely: no tier, so no clock to read.
        None => return None,
    };
    let plan_at = |key: &clank_core::ids::PlanKey| {
        snap.active_plan
            .as_ref()
            .filter(|p| &p.plan == key)
            .and_then(|p| p.handover.since(tier))
    };
    let adhoc_at = |sha: &crate::lifecycle::CommitSha| {
        snap.ad_hoc
            .iter()
            .find(|a| &a.sha == sha)
            .and_then(|a| a.handover.since(tier))
    };
    let pr_at = |pr: u32| {
        snap.pr_reviews
            .iter()
            .find(|p| p.pr == pr)
            .and_then(|p| p.handover.since(tier))
    };
    work.work_for(&parsed, role)
        .iter()
        .filter_map(|item| match item {
            WaitItem::Master { plan, .. } | WaitItem::Reviewer { plan, .. } => plan_at(plan),
            WaitItem::AdHocReview { sha, .. } | WaitItem::AdHocRevise { sha } => adhoc_at(sha),
            WaitItem::PrReviewer { pr, .. } | WaitItem::PrMaster { pr, .. } => pr_at(*pr),
            // Not obligations handed over by a review stage: a
            // notification, an idle prompt, a preempt the panel
            // surfaces elsewhere (the `fix` gauge, the queue).
            WaitItem::Idle { .. }
            | WaitItem::FixCommitTag { .. }
            | WaitItem::PromoteFromQueue { .. }
            | WaitItem::Message { .. }
            | WaitItem::GithubEvent { .. }
            | WaitItem::CommandEvent { .. }
            | WaitItem::TimerEvent { .. }
            | WaitItem::DraftChanged { .. }
            | WaitItem::ForCommit { .. }
            | WaitItem::ForFinished { .. }
            | WaitItem::ForBlocked { .. }
            | WaitItem::Blocked { .. }
            | WaitItem::Unblocked { .. } => None,
        })
        .min()
}

/// The braille spinner cycle — width-1 glyphs so it drops into the
/// verdict-mark column without disturbing alignment. One moving element,
/// per the design brief.
pub(super) const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

pub(super) fn spinner_glyph(frame: usize) -> &'static str {
    SPINNER[frame % SPINNER.len()]
}

/// In-progress activity. THE INVARIANT: whenever the gate is waiting
/// on an agent to produce something, emit an item for it (its AGENTS
/// row spins).
///
/// REVIEWER rows are ROSTER-driven through the shared
/// `is_actionable` (tui-adhoc-review-activity): one routing path
/// covers plan, PR, and AD-HOC reviews, their unions, and global
/// preemption — an ad-hoc review carries no labels, so no
/// missing-set enumeration could surface its reviewers. The verb
/// refines from the single active plan's `waiting_on` when it names
/// the reviewer ("gate-reviewing"); anything else is "reviewing".
///
/// MASTER's row keeps the single-active-plan derivation — its verbs
/// (working/revising/drafting/finalizing via [`verb_of`]) are
/// defined by plan states alone. Excluded: `Blocked` (a human's
/// turn, shown by the block ask) and `MasterToFixCommitTag`
/// (surfaced by the `fix` gauge). The match is exhaustive (no
/// catch-all) so a new producing state can't silently slip through
/// invisibly. Pure — unit-tested.
pub(super) fn in_progress_rows(snap: &StatusSnapshot) -> Vec<InProgress> {
    let plan_verb = |label: &str| -> &'static str {
        if let Some(v) = snap.active_plan.as_ref()
            && let WaitingOn::ReviewerApprovalsMissing { missing }
            | WaitingOn::GateReviewersMissing { missing } = &v.waiting_on
            && missing.iter().any(|l| l.as_str() == label)
        {
            super::derive::verb_of(&v.waiting_on)
        } else {
            "reviewing"
        }
    };
    let work = super::derive::work_projection(snap);
    let mut out: Vec<InProgress> = super::derive::awaited_reviewers(snap)
        .into_iter()
        .map(|label| {
            let verb = plan_verb(&label);
            let since = oldest_obligation(snap, &work, &label, clank_core::vocab::Role::Reviewer);
            InProgress::PendingReview { label, verb, since }
        })
        .collect();
    let master_since = |snap: &StatusSnapshot, name: &str| {
        oldest_obligation(snap, &work, name, clank_core::vocab::Role::Master)
    };
    if let Some(v) = snap.active_plan.as_ref() {
        match &v.waiting_on {
            WaitingOn::MasterToContinue
            | WaitingOn::MasterToRevise { .. }
            | WaitingOn::MasterToCommit
            | WaitingOn::MasterToFinalize => {
                let name = snap.master.as_deref().unwrap_or("master").to_string();
                out.push(InProgress::MasterWorking {
                    since: master_since(snap, &name),
                    name,
                    verb: super::derive::verb_of(&v.waiting_on),
                })
            }
            WaitingOn::ReviewerApprovalsMissing { .. }
            | WaitingOn::GateReviewersMissing { .. }
            | WaitingOn::Blocked { .. }
            | WaitingOn::MasterToFixCommitTag => {}
        }
    } else if snap
        .ad_hoc
        .iter()
        .any(|a| a.gate == clank_core::vocab::CommitGateState::ChangesRequested)
    {
        // Plan-less ad-hoc revision: master's row spins with the
        // routed verb, mirroring bar_text and master_is_active
        // (codex 4a4be39).
        let name = snap.master.as_deref().unwrap_or("master").to_string();
        out.push(InProgress::MasterWorking {
            since: master_since(snap, &name),
            name,
            verb: "revising",
        });
    }
    out
}

/// One row of scrollable content: a wrapped block-ask line or a
/// historical log row. Live activity is NOT in the sequence — it renders
/// on the AGENTS panel rows.
pub(super) enum Seg<'a> {
    Ask(&'a crate::cli::status_tui::render::AskLine),
    Log(&'a crate::cli::log::OnelineRow),
}

/// The scrollable sequence: ask lines, then the historical log rows in
/// order (review rows render ABOVE their commit, arriving first). Pure,
/// and the SINGLE source of both the rendered order and the indices the
/// Enter-targeting uses — they cannot disagree.
pub(super) fn build_scroll<'a>(
    snap: &'a StatusSnapshot,
    ask_lines: &'a [crate::cli::status_tui::render::AskLine],
) -> Vec<Seg<'a>> {
    let mut seq: Vec<Seg> = ask_lines.iter().map(Seg::Ask).collect();
    seq.extend(snap.log_rows.iter().map(Seg::Log));
    seq
}

/// What a log entry IS, independent of where it sits: rows a refresh
/// adds above the selection move its index, never this.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum EntryId {
    Commit(crate::lifecycle::CommitSha),
    /// Reviews and banners carry the commit below them; an author's
    /// verdicts recur across commits and a plan's name across runs.
    Review {
        author: String,
        at: Option<i64>,
        commit: Option<crate::lifecycle::CommitSha>,
    },
    Header {
        plan: Option<String>,
        commit: Option<crate::lifecycle::CommitSha>,
    },
    /// Any shared key is the same event: a merge can gain members.
    Event(Vec<crate::cli::github::timeline::EventKey>),
    Notice(String),
}

/// A selected entry, held across a refresh: which entry, and which of
/// its occurrences when it recurs, counted from the top so rows added
/// above it leave both alone.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct HeldEntry {
    id: EntryId,
    nth: usize,
}

/// Hold scroll entry `i`, the log's rows starting after `head` ask
/// lines. Ask lines and run-end rules have no identity to hold.
pub(super) fn hold_entry(snap: &StatusSnapshot, head: usize, i: usize) -> Option<HeldEntry> {
    let at = i.checked_sub(head)?;
    let below = snap.log_rows.get(at + 1..)?.iter().find_map(row_commit);
    let id = identify(snap, snap.log_rows.get(at)?, below)?;
    let nth = occurrences(snap, head, &id).position(|j| j == i)?;
    Some(HeldEntry { id, nth })
}

/// Where the held entry now sits, if the refresh kept it.
pub(super) fn find_entry(snap: &StatusSnapshot, head: usize, held: &HeldEntry) -> Option<usize> {
    occurrences(snap, head, &held.id).nth(held.nth)
}

fn occurrences(snap: &StatusSnapshot, head: usize, id: &EntryId) -> impl Iterator<Item = usize> {
    let same = |other: &EntryId| match (id, other) {
        (EntryId::Event(a), EntryId::Event(b)) => a.iter().any(|k| b.contains(k)),
        _ => id == other,
    };
    let mut below = None;
    let mut found = vec![];
    for (at, row) in snap.log_rows.iter().enumerate().rev() {
        if identify(snap, row, below).is_some_and(|e| same(&e)) {
            found.push(head + at);
        }
        below = row_commit(row).or(below);
    }
    found.into_iter().rev()
}

fn row_commit(row: &crate::cli::log::OnelineRow) -> Option<&crate::lifecycle::CommitSha> {
    use crate::cli::log::OnelineRow;
    match row {
        OnelineRow::Commit { sha, .. } | OnelineRow::PlainCommit { sha, .. } => Some(sha),
        _ => None,
    }
}

fn identify(
    snap: &StatusSnapshot,
    row: &crate::cli::log::OnelineRow,
    below: Option<&crate::lifecycle::CommitSha>,
) -> Option<EntryId> {
    use crate::cli::log::OnelineRow;
    Some(match row {
        OnelineRow::Commit { sha, .. } | OnelineRow::PlainCommit { sha, .. } => {
            EntryId::Commit(sha.clone())
        }
        OnelineRow::Review { author, at, .. } => EntryId::Review {
            author: author.clone(),
            at: *at,
            commit: below.cloned(),
        },
        OnelineRow::Header { plan, .. } => EntryId::Header {
            plan: plan.clone(),
            commit: below.cloned(),
        },
        OnelineRow::Github { event_idx, .. } => {
            EntryId::Event(snap.github_events.get(*event_idx)?.page_keys())
        }
        OnelineRow::Notice(text) => EntryId::Notice(text.clone()),
        OnelineRow::RunEnd => return None,
    })
}

/// Where a cursor that may have come to rest on a row nothing selects
/// (a run's end rule) lands: the nearest selectable row the way it was
/// moving, else the nearest the other way.
pub(super) fn land(
    cursor: usize,
    total: usize,
    rising: bool,
    unselectable: impl Fn(usize) -> bool,
) -> usize {
    if !unselectable(cursor) {
        return cursor;
    }
    let up = (0..cursor).rev();
    let down = cursor + 1..total;
    let found = if rising {
        up.clone()
            .find(|&i| !unselectable(i))
            .or_else(|| down.clone().find(|&i| !unselectable(i)))
    } else {
        down.clone()
            .find(|&i| !unselectable(i))
            .or_else(|| up.clone().find(|&i| !unselectable(i)))
    };
    found.unwrap_or(cursor)
}

/// The log-side half of continuous navigation: `Up` in the log crosses
/// back into the panel only when the log is FULLY AT ITS TOP — cursor
/// on the first entry AND the viewport offset at 0 — and there is a
/// roster to cross to; otherwise it moves the cursor up. The OFFSET guard matters for mouse wheels: the
/// terminal turns a wheel notch into a BURST of Ups drained in one event
/// batch, so the cursor can hit 0 while the viewport is still mid-scroll
/// (no paint has settled the offset yet) — the burst must BUMP at the
/// top, not teleport into the panel over a half-scrolled log. Pure so
/// the boundary stays tested.
pub(super) fn crosses_into_panel(cursor: usize, offset: usize, agents_len: usize) -> bool {
    cursor == 0 && offset == 0 && agents_len > 0
}

/// Derive the viewport top so the cursor entry stays visible, moving the
/// previous `offset` as little as possible and never past the last-page
/// clamp (so the final page stays full). The two constraints are
/// compatible because the caller clamps `cursor` to `total - 1` first,
/// so the offset that reveals the cursor is always ≤ the clamp.
pub(super) fn scroll_to_show(cursor: usize, offset: usize, capacity: usize, total: usize) -> usize {
    let cap = capacity.max(1);
    let max_off = total.saturating_sub(cap);
    let off = if cursor < offset {
        cursor // cursor above the window → scroll up to it
    } else if cursor >= offset + cap {
        cursor + 1 - cap // cursor below → scroll down to it
    } else {
        offset // already visible → don't move
    };
    off.min(max_off)
}

/// The page pressing Enter on a scroll entry opens.
pub(super) enum EntryTarget {
    /// A commit's page. `focus` is the reviewer whose feedback the page
    /// should open scrolled to — set when Enter lands on a `Review` row;
    /// `None` (a `Commit` row) opens at the top.
    Commit {
        sha: crate::lifecycle::CommitSha,
        focus: Option<String>,
    },
    /// A plan's page.
    Plan { key: crate::lifecycle::PlanKey },
}

/// The page a scroll entry "drills into", or `None` if it has none
/// (an ad-hoc header, an ask line, an in-progress placeholder). A
/// `Commit` seg → its own commit (no focus). A `Review` seg carries NO
/// sha (reviews are positional), so it scans FORWARD — reviews render
/// ABOVE their commit — to the next `Commit`, but STOPS at a section
/// `Header` or the end of the sequence: a stray or tail review (which
/// `build_scroll` flushes with no following commit in its section) must
/// NOT bind to the next section's commit. The review's author rides along
/// as the scroll focus. A plan `Header` → that plan's page.
pub(super) fn entry_target(seq: &[Seg], cursor: usize) -> Option<EntryTarget> {
    use crate::cli::log::OnelineRow;
    match seq.get(cursor)? {
        Seg::Log(OnelineRow::Commit { sha, .. })
        | Seg::Log(OnelineRow::PlainCommit { sha, .. }) => Some(EntryTarget::Commit {
            sha: sha.clone(),
            focus: None,
        }),
        Seg::Log(OnelineRow::Review { author, .. }) => {
            let sha = seq[cursor + 1..]
                .iter()
                .find_map(|s| match s {
                    Seg::Log(OnelineRow::Commit { sha, .. }) => Some(Some(sha.clone())),
                    Seg::Log(OnelineRow::Header { .. }) => Some(None), // section break
                    _ => None,                                         // skip reviews
                })
                .flatten()?;
            Some(EntryTarget::Commit {
                sha,
                focus: Some(author.clone()),
            })
        }
        Seg::Log(OnelineRow::Header {
            plan: Some(stem), ..
        }) => crate::lifecycle::PlanKey::parse(stem)
            .ok()
            .map(|key| EntryTarget::Plan { key }),
        _ => None, // ad-hoc header, ask, in-progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::status_tui::fixtures::{plan_state, reviewer_missing, snap};
    use crate::cli::status_tui::render::block_ask_spans;
    use crate::cli::status_tui::text::display_width;

    /// Enter across a run's end rule: the rule opens nothing, a plan
    /// banner still opens its plan, and a review under the rule still
    /// opens the finish it reviews.
    #[test]
    fn a_run_end_rule_opens_nothing_and_binds_nothing() {
        use crate::cli::log::{OnelineRow, RowMarker};
        let sha = crate::lifecycle::CommitSha::parse(&"f".repeat(40)).unwrap();
        let rows = [
            OnelineRow::RunEnd,
            OnelineRow::Review {
                verdict: clank_core::vocab::Verdict::Finished,
                author: "codex".into(),
                summary: String::new(),
                at: None,
            },
            OnelineRow::Commit {
                sha: sha.clone(),
                at: 0,
                subject: "finish".into(),
                marker: RowMarker::Finish,
                stands_for: None,
            },
            OnelineRow::Header {
                plan: Some("p".into()),
                duration_secs: None,
                starts_here: true,
            },
        ];
        let seq: Vec<Seg> = rows.iter().map(Seg::Log).collect();
        assert!(entry_target(&seq, 0).is_none());
        assert!(matches!(
            entry_target(&seq, 1),
            Some(EntryTarget::Commit { sha: s, focus: Some(f) }) if s == sha && f == "codex"
        ));
        assert!(matches!(
            entry_target(&seq, 3),
            Some(EntryTarget::Plan { .. })
        ));
    }

    #[test]
    fn up_crosses_into_the_panel_only_when_fully_topped() {
        assert!(crosses_into_panel(0, 0, 2), "fully topped → cross");
        assert!(!crosses_into_panel(3, 2, 2), "mid-log → move cursor up");
        // The mouse-wheel-burst state: cursor already walked to 0 within
        // one drained batch, but no paint has settled the offset yet —
        // the burst must BUMP at the top, never cross over a
        // half-scrolled log.
        assert!(!crosses_into_panel(0, 4, 2), "viewport mid-scroll → bump");
        assert!(
            !crosses_into_panel(0, 0, 0),
            "no roster → nothing to cross to"
        );
    }

    #[test]
    fn scroll_to_show_follows_cursor_and_respects_last_page() {
        // Already visible → don't move.
        assert_eq!(scroll_to_show(3, 2, 5, 20), 2);
        // Above the window → scroll up onto it.
        assert_eq!(scroll_to_show(1, 5, 5, 20), 1);
        // Below the window → scroll down so it's the last visible row.
        assert_eq!(scroll_to_show(9, 2, 5, 20), 5);
        // Last-page clamp AND cursor-visible together: total 12, cap 5 →
        // max_off 7; a cursor near the end can't push offset past 7, and
        // is STILL inside the painted window.
        let off = scroll_to_show(11, 0, 5, 12);
        assert_eq!(off, 7, "clamped to the last page");
        assert!(
            (off..off + 5).contains(&11),
            "cursor still painted under the clamp"
        );
        // Log shorter than the viewport → offset 0, cursor visible.
        assert_eq!(scroll_to_show(2, 0, 10, 3), 0);
    }

    #[test]
    fn cursor_derived_window_tracks_spinner_visibility() {
        // The loop computes spinner-visibility from `offset..offset+cap`
        // with the CURSOR-derived offset. So an in-progress row at seq
        // index 1 is "visible" only while the cursor keeps it in the
        // window — scrolling the cursor away takes it out (no wasted
        // animation ticks), and back in resumes them.
        let cap = 5;
        let total = 30;
        let near = scroll_to_show(2, 0, cap, total);
        assert!(
            (near..near + cap).contains(&1),
            "spinner in view near the top"
        );
        let far = scroll_to_show(25, near, cap, total);
        assert!(
            !(far..far + cap).contains(&1),
            "spinner scrolled off → window excludes it"
        );
    }

    #[test]
    fn spinner_glyph_cycles_and_is_width_one() {
        assert_eq!(spinner_glyph(0), spinner_glyph(SPINNER.len()), "wraps");
        assert_ne!(spinner_glyph(0), spinner_glyph(1), "advances");
        for f in 0..SPINNER.len() {
            assert_eq!(display_width(spinner_glyph(f)), 1, "fits the mark column");
        }
    }

    #[test]
    fn in_progress_rows_derive_from_waiting_on() {
        use crate::cli::status_tui::fixtures::with_agents;
        use crate::cli::teams_config::RosterRole;
        let roster: &[(&str, RosterRole)] = &[
            ("claude", RosterRole::Master),
            ("codex", RosterRole::Commit),
        ];
        // Pending reviewers → one spinner row each, verb "reviewing".
        let s = with_agents(
            snap(Some(plan_state("foo", reviewer_missing("codex"))), vec![]),
            roster,
        );
        assert!(matches!(
            in_progress_rows(&s).as_slice(),
            [InProgress::PendingReview { label, verb, .. }]
                if label == "codex" && *verb == "reviewing"
        ));
        // Master producing the next commit → one master row, per-state verb.
        let s = snap(Some(plan_state("foo", WaitingOn::MasterToContinue)), vec![]);
        assert!(matches!(
            in_progress_rows(&s).as_slice(),
            [InProgress::MasterWorking { verb, .. }] if *verb == "working"
        ));
        // FLIP (reproduce-first): finalizing IS making the finish commit,
        // so it now produces a master row — shipped M2 wrongly returned
        // none for MasterToFinalize.
        let s = snap(Some(plan_state("foo", WaitingOn::MasterToFinalize)), vec![]);
        assert!(matches!(
            in_progress_rows(&s).as_slice(),
            [InProgress::MasterWorking { verb, .. }] if *verb == "finalizing"
        ));
        // MasterToFixCommitTag → surfaced by the `fix` gauge, no spinner.
        let s = snap(
            Some(plan_state("foo", WaitingOn::MasterToFixCommitTag)),
            vec![],
        );
        assert!(in_progress_rows(&s).is_empty());
    }

    /// The stage boundaries an agent's clock reads: the work appeared
    /// at 100, the commit tier closed at 200, the second tier at 300.
    fn stages(
        commit_tier_closed: Option<i64>,
        second_tier_closed: Option<i64>,
    ) -> clank_core::wait::Handover {
        clank_core::wait::Handover {
            opened: Some(100),
            commit_tier_closed,
            second_tier_closed,
        }
    }

    fn since_of(rows: &[InProgress], who: &str) -> Option<i64> {
        rows.iter()
            .find(|p| match p {
                InProgress::PendingReview { label, .. } => label == who,
                InProgress::MasterWorking { name, .. } => name == who,
            })
            .and_then(|p| p.since())
    }

    /// Each agent's clock starts at the boundary of the stage that
    /// summoned it — which is a DIFFERENT boundary per tier, on the
    /// same plan: the commit tier answers the commit, the gate tier
    /// answers the commit tier closing. Collapsing the two would hand
    /// a gate reviewer the whole commit review's time the moment it
    /// was asked.
    #[test]
    fn an_agents_clock_starts_at_the_stage_that_summoned_it() {
        use crate::cli::status_tui::fixtures::{plan_state_at, with_agents};
        use crate::cli::teams_config::RosterRole;
        let roster: &[(&str, RosterRole)] = &[
            ("claude", RosterRole::Master),
            ("codex", RosterRole::Commit),
            ("ruthless", RosterRole::Gate),
        ];
        // The commit tier still owes: codex has owed since the commit.
        let s = with_agents(
            snap(
                Some(plan_state_at(
                    "foo",
                    reviewer_missing("codex"),
                    stages(None, None),
                )),
                vec![],
            ),
            roster,
        );
        assert_eq!(since_of(&in_progress_rows(&s), "codex"), Some(100));

        // The commit tier closed at 200 and summoned the gate tier:
        // ruthless has owed since 200, not since the commit.
        let s = with_agents(
            snap(
                Some(plan_state_at(
                    "foo",
                    WaitingOn::GateReviewersMissing {
                        missing: clank_core::repo_state::NonEmptyVec::new(vec![
                            clank_core::ids::AgentLabel::parse("ruthless").unwrap(),
                        ])
                        .unwrap(),
                    },
                    stages(Some(200), None),
                )),
                vec![],
            ),
            roster,
        );
        assert_eq!(since_of(&in_progress_rows(&s), "ruthless"), Some(200));

        // Master was handed the plan back by the last tier to close.
        let s = with_agents(
            snap(
                Some(plan_state_at(
                    "foo",
                    WaitingOn::MasterToContinue,
                    stages(Some(200), Some(300)),
                )),
                vec![],
            ),
            roster,
        );
        assert_eq!(since_of(&in_progress_rows(&s), "claude"), Some(300));
        // With no second tier consulted, the commit tier's close is
        // what handed it back.
        let s = with_agents(
            snap(
                Some(plan_state_at(
                    "foo",
                    WaitingOn::MasterToContinue,
                    stages(Some(200), None),
                )),
                vec![],
            ),
            roster,
        );
        assert_eq!(since_of(&in_progress_rows(&s), "claude"), Some(200));
    }

    /// A reviewer awaited by a PR round reads its clock from that
    /// round's stages — the same rule over a different work item.
    #[test]
    fn a_pr_reviewer_is_dated_by_the_rounds_stages() {
        use crate::cli::status_tui::fixtures::{pr_awaiting, with_agents};
        use crate::cli::teams_config::RosterRole;
        let mut s = with_agents(
            snap(None, vec![]),
            &[
                ("claude", RosterRole::Master),
                ("codex", RosterRole::Commit),
            ],
        );
        let mut pr = pr_awaiting(&["codex"]);
        pr.handover = clank_core::wait::Handover {
            opened: Some(4_242),
            ..Default::default()
        };
        s.pr_reviews = vec![pr];
        assert_eq!(since_of(&in_progress_rows(&s), "codex"), Some(4_242));
    }

    /// An agent owing two things has been owing since the OLDER one:
    /// the elapsed answers "how long has this agent had work waiting",
    /// which the newer obligation cannot shorten.
    #[test]
    fn two_obligations_show_the_older() {
        use crate::cli::status_tui::fixtures::{plan_state_at, with_agents};
        use crate::cli::teams_config::RosterRole;
        let old = clank_core::wait::Handover {
            opened: Some(100),
            ..Default::default()
        };
        let recent = clank_core::wait::Handover {
            opened: Some(9_000),
            ..Default::default()
        };
        let mut s = with_agents(
            snap(
                Some(plan_state_at("newer", reviewer_missing("codex"), recent)),
                vec![],
            ),
            &[
                ("claude", RosterRole::Master),
                ("codex", RosterRole::Commit),
            ],
        );
        let mut pr = crate::cli::status_tui::fixtures::pr_awaiting(&["codex"]);
        pr.handover = old;
        s.pr_reviews.push(pr);
        assert_eq!(since_of(&in_progress_rows(&s), "codex"), Some(100));
    }

    /// A work state with no dated stage draws no elapsed at all — and
    /// the row still spins, because whether an agent is working and
    /// whether we can date it are two questions.
    #[test]
    fn an_undated_obligation_still_spins() {
        use crate::cli::status_tui::fixtures::{plan_state_at, with_agents};
        use crate::cli::teams_config::RosterRole;
        let s = with_agents(
            snap(
                Some(plan_state_at(
                    "foo",
                    reviewer_missing("codex"),
                    clank_core::wait::Handover::default(),
                )),
                vec![],
            ),
            &[
                ("claude", RosterRole::Master),
                ("codex", RosterRole::Commit),
            ],
        );
        let rows = in_progress_rows(&s);
        assert_eq!(rows.len(), 1, "still spinning");
        assert_eq!(since_of(&rows, "codex"), None);
    }

    // A log of [Header(foo), Commit(latest reviewable sha)] for the active
    // plan `foo`, used to check WHERE placeholders get spliced.
    fn snap_with_header_and_commit(waiting: WaitingOn) -> StatusSnapshot {
        use crate::cli::log::OnelineRow;
        let mut s = snap(Some(plan_state("foo", waiting)), vec![]);
        let sha = s.active_plan.as_ref().unwrap().sha.clone().unwrap();
        s.log_rows = vec![
            OnelineRow::Header {
                duration_secs: None,
                starts_here: true,
                plan: Some("foo".into()),
            },
            OnelineRow::Commit {
                at: 0,
                sha,
                subject: "do a thing".into(),
                marker: crate::cli::log::RowMarker::Plain,

                stands_for: None,
            },
        ];
        s
    }

    fn seg_kind(s: &Seg) -> &'static str {
        use crate::cli::log::OnelineRow;
        match s {
            Seg::Ask(_) => "ask",
            Seg::Log(OnelineRow::Header { .. }) => "header",
            Seg::Log(OnelineRow::Commit { .. }) => "commit",
            Seg::Log(_) => "log",
        }
    }

    #[test]
    fn scroll_sequence_is_pure_history_no_placeholders() {
        // Activity renders on the AGENTS panel rows
        // (tui-spinner-on-agent-rows) — the scroll sequence carries ONLY
        // asks + historical log rows, even while the gate waits on a
        // reviewer or on master. The "waiting is never invisible"
        // invariant now lives in the agent-row render test.
        for waiting in [reviewer_missing("codex"), WaitingOn::MasterToContinue] {
            let s = crate::cli::status_tui::fixtures::with_agents(
                snap_with_header_and_commit(waiting),
                &[
                    ("claude", crate::cli::teams_config::RosterRole::Master),
                    ("codex", crate::cli::teams_config::RosterRole::Commit),
                ],
            );
            let ask = block_ask_spans(&s, 80);
            assert!(
                !in_progress_rows(&s).is_empty(),
                "the gate IS waiting on someone"
            );
            let kinds: Vec<&str> = build_scroll(&s, &ask).iter().map(seg_kind).collect();
            assert_eq!(kinds, ["header", "commit"], "history only, no placeholder");
            // The loop's cursor/fill accounting assumes EXACTLY this
            // length (head = asks; total = asks + history) — no phantom
            // rows beyond asks + history while activity is pending
            // (codex 5482f26).
            assert_eq!(
                build_scroll(&s, &ask).len(),
                ask.len() + s.log_rows.len(),
                "scrollable total is asks + history, nothing more"
            );
        }
    }

    /// Rows from the PRODUCER, not hand-built ones: the fixtures above
    /// place the name on top, so they cannot tell whether navigation
    /// survives the name moving under its run (codex on 51fb3c8).
    ///
    /// Two plans in sequence, each with a review over its newest
    /// commit. A review must reach its own commit, and the name must
    /// still open its own plan.
    #[test]
    fn navigation_survives_the_name_moving_under_its_run() {
        use crate::cli::log::{OnelineRow, oneline_rows};
        use crate::lifecycle::{CommitSha, PlanKey};
        use clank_core::repo_state::LogEvent;
        use clank_core::vocab::Verdict;
        let sha = |s: &str| CommitSha::parse(&format!("{:0<40}", s)).unwrap();
        let commit = |plan: &str, s: &str, ts: i64| LogEvent::PlanCommit {
            plan: PlanKey::parse(plan).unwrap(),
            sha: sha(s),
            ts,
            touched_plan: false,
            touched_code: true,
            subject: format!("[{plan}] work"),
        };
        // Newest-first, as the log renders: plan `bee` then plan `ant`.
        let events = [
            commit("bee", "b2", 4),
            commit("bee", "b1", 3),
            commit("ant", "a2", 2),
            commit("ant", "a1", 1),
        ];
        let refs: Vec<&LogEvent> = events.iter().collect();
        let mut reviews = std::collections::BTreeMap::new();
        for (s, who) in [("b2", "codex"), ("a2", "kimi")] {
            reviews.insert(
                sha(s).as_str().to_string(),
                vec![crate::cli::log::Review {
                    at: None,
                    author: who.into(),
                    verdict: Verdict::Continue,
                    summary: "ok".into(),
                    body: String::new(),
                }],
            );
        }
        let rows = oneline_rows(&refs, &reviews, &Default::default());
        // The shape under test: each run's name closes it.
        let shape: Vec<&str> = rows
            .iter()
            .map(|r| match r {
                OnelineRow::Header { plan, .. } => plan.as_deref().unwrap_or("adhoc"),
                OnelineRow::Review { .. } => "review",
                _ => "commit",
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                "review", "commit", "commit", "bee", "review", "commit", "commit", "ant"
            ],
            "{rows:?}"
        );

        let seq: Vec<Seg> = rows.iter().map(Seg::Log).collect();
        let commit_of = |t: Option<EntryTarget>| match t {
            Some(EntryTarget::Commit { sha, focus }) => Some((sha, focus)),
            _ => None,
        };
        // Each review reaches ITS OWN commit, not the neighbouring
        // plan's — the failure a misplaced section break causes.
        assert_eq!(
            commit_of(entry_target(&seq, 0)),
            Some((sha("b2"), Some("codex".to_string()))),
            "the first review targets the newest commit of its own run"
        );
        assert_eq!(
            commit_of(entry_target(&seq, 4)),
            Some((sha("a2"), Some("kimi".to_string()))),
            "and the review after a name targets the run BELOW it"
        );
        // Each name still opens its own plan's document.
        assert!(matches!(
            entry_target(&seq, 3),
            Some(EntryTarget::Plan { key }) if key.as_str() == "bee"
        ));
        assert!(matches!(
            entry_target(&seq, 7),
            Some(EntryTarget::Plan { key }) if key.as_str() == "ant"
        ));
    }

    #[test]
    fn entry_target_resolves_positionally() {
        use crate::cli::log::OnelineRow;
        use crate::lifecycle::CommitSha;
        use clank_core::vocab::Verdict;
        let sha = |s: &str| CommitSha::parse(&format!("{:0<40}", s)).unwrap();
        let commit = |s: &str| OnelineRow::Commit {
            at: 0,
            sha: sha(s),
            subject: "x".into(),
            marker: crate::cli::log::RowMarker::Plain,

            stands_for: None,
        };
        let review = |a: &str| OnelineRow::Review {
            at: None,
            verdict: Verdict::Continue,
            author: a.into(),
            summary: "ok".into(),
        };
        let header = || OnelineRow::Header {
            duration_secs: None,
            starts_here: true,
            plan: Some("foo".into()),
        };

        // Helpers to read the resolved target.
        let commit_of = |t: Option<EntryTarget>| match t {
            Some(EntryTarget::Commit { sha, focus }) => Some((sha, focus)),
            _ => None,
        };

        // Header, Review(aaa), Commit(c1), Header, Review(bbb) [tail].
        let rows = [
            header(),
            review("aaa"),
            commit("c1"),
            header(),
            review("bbb"),
        ];
        let seq: Vec<Seg> = rows.iter().map(Seg::Log).collect();
        // A commit row → its own sha, no review focus.
        assert_eq!(commit_of(entry_target(&seq, 2)), Some((sha("c1"), None)));
        // A review directly above its commit → that commit, focused on the
        // review's author.
        assert_eq!(
            commit_of(entry_target(&seq, 1)),
            Some((sha("c1"), Some("aaa".to_string())))
        );
        // A plan header → that plan's document.
        assert!(matches!(
            entry_target(&seq, 0),
            Some(EntryTarget::Plan { key }) if key.as_str() == "foo"
        ));
        // A TAIL review with no following commit → None (not a panic, not
        // a bind to some earlier commit).
        assert!(entry_target(&seq, 4).is_none());
        // Out-of-range cursor → None.
        assert!(entry_target(&seq, 99).is_none());

        // An ad-hoc header (plan: None) has no document.
        let adhoc = [OnelineRow::Header {
            duration_secs: None,
            starts_here: true,
            plan: None,
        }];
        let seq_adhoc: Vec<Seg> = adhoc.iter().map(Seg::Log).collect();
        assert!(entry_target(&seq_adhoc, 0).is_none());

        // A stray review whose section ends at a Header before any commit
        // must NOT bind to the NEXT section's commit (the wrong-entry bug).
        let rows2 = [review("zzz"), header(), commit("c2")];
        let seq2: Vec<Seg> = rows2.iter().map(Seg::Log).collect();
        assert!(
            entry_target(&seq2, 0).is_none(),
            "review before a section break stops at the header"
        );
    }
}

/// What divides the (lifted) header from the log entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Divider {
    /// The focusable LOG region rule (panel present).
    Rule,
    /// The panel-less blank separator.
    Separator,
    None,
}

/// The log region's row budget — THE single owner of the pane's row
/// arithmetic. `render_at` DRAWS this decision and the event loop
/// settles the log window against the same numbers, so the two can't
/// drift (they did, three review rounds running — codex be2b054,
/// ce616ce lineage).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LogBudget {
    pub(super) divider: Divider,
    /// Log ENTRY rows (after the divider's row, if any).
    pub(super) capacity: usize,
}

/// Budget the pane: a pinned bar row, then the header's visible
/// remainder (its length minus `lift`, capped to the pane), then the
/// log region. The rule YIELDS its row when it would leave no entry
/// row for a focused selection (rows == 2 shape): selection
/// visibility outranks the rule, the bar outranks both.
pub(super) fn log_budget(
    rows: usize,
    header_len: usize,
    lift: usize,
    has_panel: bool,
    log_focused: bool,
    total: usize,
) -> LogBudget {
    let rows = rows.max(1);
    let visible_header = (header_len.saturating_sub(lift)).min(rows - 1);
    let mut avail = rows - 1 - visible_header;
    // The region renders when it has content OR a panel to switch
    // focus with; otherwise it is absent entirely.
    if avail == 0 || (total == 0 && !has_panel) {
        return LogBudget {
            divider: Divider::None,
            capacity: 0,
        };
    }
    let rule_yields = log_focused && avail == 1 && total > 0;
    let divider = if has_panel && !rule_yields {
        avail -= 1;
        Divider::Rule
    } else if !has_panel && avail >= 2 {
        avail -= 1;
        Divider::Separator
    } else {
        Divider::None
    };
    LogBudget {
        divider,
        capacity: avail,
    }
}

/// Whole-pane pressure offset (tui-short-pane-whole-scroll): the
/// SMALLEST lift whose [`log_budget`] meets the entry target — the
/// policy optimizes over the render's own budget function, so the two
/// cannot disagree by construction. Pure.
///
/// - Unfocused, or an empty sequence: 0 — the header never moves under
///   you, and an empty timeline reserves nothing (codex ce616ce).
/// - The target is `1 + cursor` (one entry visible even at
///   cursor-at-top — with an over-tall header that means the header is
///   partially hidden from the start; selection visibility outranks
///   the full header), capped at the entries that exist and what ANY
///   lift can achieve at this height (rows == 1: nothing can — the bar
///   owns the pane, the stated degradation). Uncapped otherwise, so
///   descending the log lifts the header away row by row until the log
///   has the pane under the bar, and climbing back brings it back, as a
///   plan page's document does.
pub(super) fn pressure_lift(
    log_focused: bool,
    rows: usize,
    header_len: usize,
    cursor: usize,
    total: usize,
) -> usize {
    if !log_focused || total == 0 {
        return 0;
    }
    let cursor = cursor.min(total - 1);
    let budget_at = |lift: usize| log_budget(rows, header_len, lift, true, true, total);
    let max_capacity = budget_at(header_len).capacity;
    let target = (1 + cursor).min(total).min(max_capacity);
    if target == 0 {
        return 0;
    }
    // Prefer a lift that keeps the RULE alongside the target (it
    // carries the focus affordance — pinned whenever affordable);
    // fall back to the yield shape only when no lift can afford both
    // (the rows == 2 family).
    (0..=header_len)
        .find(|&lift| {
            let b = budget_at(lift);
            b.divider == Divider::Rule && b.capacity >= target
        })
        .or_else(|| (0..=header_len).find(|&lift| budget_at(lift).capacity >= target))
        .unwrap_or(0)
}
