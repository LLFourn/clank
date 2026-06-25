//! The scrollable-timeline model: the live in-progress placeholders
//! synthesized from the active plan's `WaitingOn`, the [`Seg`]uence that
//! splices them into the historical log at their final slots, and the
//! cursor→viewport math. Pure (no rendering, no IO) — the renderer turns
//! each [`Seg`] into spans, and the loop drives the cursor.

use super::derive::verb_of;
use super::text::Span;
use crate::cli::status::StatusSnapshot;
use clank_core::plan_view::WaitingOn;

/// A live, in-flight timeline row synthesized from the active plan's
/// `WaitingOn` (NOT from `log_rows`, which is historical). These sit at
/// the TOP of the timeline and carry the only animated glyph on screen.
pub(super) enum InProgress {
    /// A registered reviewer who still owes a verdict on the latest
    /// reviewable commit — a spinner where their ✓/✗ will land. `verb`
    /// is the italic wait-text ("reviewing" / "gate-reviewing").
    PendingReview { label: String, verb: &'static str },
    /// Master is producing the next commit — a spinner where that commit
    /// will land. `verb` says which (working/revising/committing/
    /// finalizing) via [`verb_of`].
    MasterWorking { name: String, verb: &'static str },
}

/// The braille spinner cycle — width-1 glyphs so it drops into the
/// verdict-mark column without disturbing alignment. One moving element,
/// per the design brief.
pub(super) const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

pub(super) fn spinner_glyph(frame: usize) -> &'static str {
    SPINNER[frame % SPINNER.len()]
}

/// In-progress rows for the active plan. THE INVARIANT: whenever the gate
/// is waiting on an agent to produce something, emit a placeholder for it.
/// Pending reviewers each get a spinner row; any master-producing state —
/// incl. `MasterToFinalize` (the finish commit) — gets one master row.
/// Excluded: `Blocked` (a human's turn, shown by the block ask) and
/// `MasterToFixCommitTag` (surfaced by the `fix` gauge). The match is
/// exhaustive (no catch-all) so a new producing state can't silently slip
/// through without a placeholder. Pure — unit-tested.
pub(super) fn in_progress_rows(snap: &StatusSnapshot) -> Vec<InProgress> {
    let [v] = snap.plans.as_slice() else {
        return Vec::new();
    };
    let verb = verb_of(&v.waiting_on);
    match &v.waiting_on {
        WaitingOn::ReviewerApprovalsMissing { missing }
        | WaitingOn::GateReviewersMissing { missing } => missing
            .iter()
            .map(|l| InProgress::PendingReview {
                label: l.as_str().to_string(),
                verb,
            })
            .collect(),
        WaitingOn::MasterToContinue
        | WaitingOn::MasterToRevise { .. }
        | WaitingOn::MasterToCommit
        | WaitingOn::MasterToFinalize => vec![InProgress::MasterWorking {
            name: snap.master.as_deref().unwrap_or("master").to_string(),
            verb,
        }],
        WaitingOn::Blocked { .. } | WaitingOn::MasterToFixCommitTag => Vec::new(),
    }
}

/// One row of scrollable content: a wrapped block-ask line, a historical
/// log row, or a live in-progress placeholder.
pub(super) enum Seg<'a> {
    Ask(&'a Vec<Span>),
    Log(&'a crate::cli::log::OnelineRow),
    InProg(&'a InProgress),
}

/// The scrollable sequence: ask lines, then the log with in-progress
/// placeholders SPLICED into their final slots — a master row right after
/// the active plan's umbrella header (its next commit), pending reviews
/// into the latest reviewable commit's review block (above that commit).
/// So each placeholder sits exactly where its real row will appear and is
/// replaced in place when the work lands. Pure, and the SINGLE source of
/// both the rendered order and the in-progress indices the tick checks.
pub(super) fn build_scroll<'a>(
    snap: &'a StatusSnapshot,
    ask_lines: &'a [Vec<Span>],
    in_prog: &'a [InProgress],
) -> Vec<Seg<'a>> {
    use crate::cli::log::OnelineRow;
    let mut seq: Vec<Seg> = ask_lines.iter().map(Seg::Ask).collect();

    let master: Vec<&InProgress> = in_prog
        .iter()
        .filter(|r| matches!(r, InProgress::MasterWorking { .. }))
        .collect();
    let pending: Vec<&InProgress> = in_prog
        .iter()
        .filter(|r| matches!(r, InProgress::PendingReview { .. }))
        .collect();
    let active = snap.plans.first();
    let active_stem = active.map(|v| v.plan.as_str());
    let review_sha = active.and_then(|v| v.sha.as_ref());

    let mut master_done = false;
    let mut pending_done = false;
    // Review rows render ABOVE their commit (M1), so they arrive before
    // the commit; buffer them and flush the whole block at the commit.
    let mut review_buf: Vec<&OnelineRow> = Vec::new();
    for row in &snap.log_rows {
        match row {
            OnelineRow::Review { .. } => review_buf.push(row),
            OnelineRow::Commit { sha, .. } => {
                if review_sha == Some(sha) && !pending.is_empty() {
                    // MERGE pending placeholders INTO the review block in
                    // the same author order the done reviews use (entries
                    // is a BTreeMap by AgentLabel), so a pending reviewer
                    // occupies the EXACT slot its finished ✓/✗ will take —
                    // replaced in place, never moved.
                    seq.extend(merge_review_block(&review_buf, &pending));
                    pending_done = true;
                } else {
                    seq.extend(review_buf.iter().map(|r| Seg::Log(r)));
                }
                review_buf.clear();
                seq.push(Seg::Log(row));
            }
            OnelineRow::Header { plan } => {
                // flush any stray buffered reviews across a section break
                seq.extend(review_buf.drain(..).map(Seg::Log));
                seq.push(Seg::Log(row));
                if !master_done && !master.is_empty() && plan.as_deref() == active_stem {
                    seq.extend(master.iter().map(|m| Seg::InProg(m)));
                    master_done = true;
                }
            }
        }
    }
    seq.extend(review_buf.iter().map(|r| Seg::Log(r)));
    // Anchor not found (e.g. no commit/header yet) → still show the
    // placeholder so a waiting state is never invisible.
    if !master_done {
        seq.extend(master.iter().map(|m| Seg::InProg(m)));
    }
    if !pending_done {
        seq.extend(merge_review_block(&[], &pending));
    }
    seq
}

/// Merge a commit's finished review rows with its pending-reviewer
/// placeholders into ONE block, sorted by author label — the same order
/// `collect_reviews` emits done reviews (its `entries` is a
/// `BTreeMap<AgentLabel, _>`). So a pending reviewer sits exactly where
/// its finished row will land, and the spinner is replaced in place.
fn merge_review_block<'a>(
    done: &[&'a crate::cli::log::OnelineRow],
    pending: &[&'a InProgress],
) -> Vec<Seg<'a>> {
    let mut block: Vec<(&str, Seg<'a>)> = Vec::new();
    for r in done {
        if let crate::cli::log::OnelineRow::Review { author, .. } = r {
            block.push((author.as_str(), Seg::Log(r)));
        }
    }
    for p in pending {
        if let InProgress::PendingReview { label, .. } = p {
            block.push((label.as_str(), Seg::InProg(p)));
        }
    }
    block.sort_by(|a, b| a.0.cmp(b.0));
    block.into_iter().map(|(_, seg)| seg).collect()
}

/// The log-side half of continuous navigation: pressing `Up` in the
/// log returns `Some(panel_row)` when the CURSOR is on the first entry
/// (cross back into the panel, landing on the "+ add" row adjacent to
/// the log), else `None` (move the cursor up). Pure so the boundary
/// stays tested.
pub(super) fn log_up_target(cursor: usize, agents_len: usize) -> Option<usize> {
    (cursor == 0 && agents_len > 0).then_some(agents_len)
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
