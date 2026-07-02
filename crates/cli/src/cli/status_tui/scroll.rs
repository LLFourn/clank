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
/// log returns `Some(panel_row)` when the log is FULLY AT ITS TOP —
/// cursor on the first entry AND the viewport offset at 0 — (cross back
/// into the panel, landing on the row adjacent to the log), else `None`
/// (move the cursor up). The OFFSET guard matters for mouse wheels: the
/// terminal turns a wheel notch into a BURST of Ups drained in one event
/// batch, so the cursor can hit 0 while the viewport is still mid-scroll
/// (no paint has settled the offset yet) — the burst must BUMP at the
/// top, not teleport into the panel over a half-scrolled log. Pure so
/// the boundary stays tested.
pub(super) fn log_up_target(cursor: usize, offset: usize, agents_len: usize) -> Option<usize> {
    (cursor == 0 && offset == 0 && agents_len > 0).then_some(agents_len)
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

/// What pressing Enter on a scroll entry opens, or `None` for an entry
/// with no document (an ad-hoc header, an ask line, an in-progress
/// placeholder).
pub(super) enum OverlayTarget {
    /// A commit's detail view. `focus` is the reviewer whose feedback the
    /// view should open scrolled to — set when Enter lands on a `Review`
    /// row; `None` (a `Commit` row) opens at the top.
    Commit {
        sha: crate::lifecycle::CommitSha,
        focus: Option<String>,
    },
    /// A plan's rendered markdown document, by stem.
    Plan { stem: String },
}

/// The document a scroll entry "drills into", or `None` if it has none
/// (an ad-hoc header, an ask line, an in-progress placeholder). A
/// `Commit` seg → its own commit (no focus). A `Review` seg carries NO
/// sha (reviews are positional), so it scans FORWARD — reviews render
/// ABOVE their commit — to the next `Commit`, but STOPS at a section
/// `Header` or the end of the sequence: a stray or tail review (which
/// `build_scroll` flushes with no following commit in its section) must
/// NOT bind to the next section's commit. The review's author rides along
/// as the scroll focus. A plan `Header` → that plan's document.
pub(super) fn entry_overlay_target(seq: &[Seg], cursor: usize) -> Option<OverlayTarget> {
    use crate::cli::log::OnelineRow;
    match seq.get(cursor)? {
        Seg::Log(OnelineRow::Commit { sha, .. }) => Some(OverlayTarget::Commit {
            sha: sha.clone(),
            focus: None,
        }),
        Seg::Log(OnelineRow::Review { author, .. }) => {
            let sha = seq[cursor + 1..]
                .iter()
                .find_map(|s| match s {
                    Seg::Log(OnelineRow::Commit { sha, .. }) => Some(Some(sha.clone())),
                    Seg::Log(OnelineRow::Header { .. }) => Some(None), // section break
                    _ => None,                                         // skip reviews/in-progress
                })
                .flatten()?;
            Some(OverlayTarget::Commit {
                sha,
                focus: Some(author.clone()),
            })
        }
        Seg::Log(OnelineRow::Header { plan: Some(stem) }) => {
            Some(OverlayTarget::Plan { stem: stem.clone() })
        }
        _ => None, // ad-hoc header, ask, in-progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::status_tui::fixtures::{plan_state, reviewer_missing, snap};
    use crate::cli::status_tui::render::block_ask_spans;
    use crate::cli::status_tui::text::display_width;

    #[test]
    fn log_up_target_crosses_to_panel_only_when_fully_topped() {
        // Cursor on the first entry AND viewport at the top: Up crosses
        // back to the panel row adjacent to the log (index ==
        // agents.len()); otherwise it moves the cursor up (None).
        assert_eq!(log_up_target(0, 0, 2), Some(2), "fully topped → cross");
        assert_eq!(log_up_target(3, 2, 2), None, "mid-log → move cursor up");
        // The mouse-wheel-burst state: cursor already walked to 0 within
        // one drained batch, but no paint has settled the offset yet —
        // the burst must BUMP at the top, never cross over a
        // half-scrolled log.
        assert_eq!(log_up_target(0, 4, 2), None, "viewport mid-scroll → bump");
        assert_eq!(
            log_up_target(0, 0, 0),
            None,
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
        // Pending reviewers → one spinner row each, verb "reviewing".
        let s = snap(vec![plan_state("foo", reviewer_missing("codex"))], vec![]);
        assert!(matches!(
            in_progress_rows(&s).as_slice(),
            [InProgress::PendingReview { label, verb }] if label == "codex" && *verb == "reviewing"
        ));
        // Master producing the next commit → one master row, per-state verb.
        let s = snap(vec![plan_state("foo", WaitingOn::MasterToContinue)], vec![]);
        assert!(matches!(
            in_progress_rows(&s).as_slice(),
            [InProgress::MasterWorking { verb, .. }] if *verb == "working"
        ));
        // FLIP (reproduce-first): finalizing IS making the finish commit,
        // so it now produces a master row — shipped M2 wrongly returned
        // none for MasterToFinalize.
        let s = snap(vec![plan_state("foo", WaitingOn::MasterToFinalize)], vec![]);
        assert!(matches!(
            in_progress_rows(&s).as_slice(),
            [InProgress::MasterWorking { verb, .. }] if *verb == "finalizing"
        ));
        // MasterToFixCommitTag → surfaced by the `fix` gauge, no spinner.
        let s = snap(
            vec![plan_state("foo", WaitingOn::MasterToFixCommitTag)],
            vec![],
        );
        assert!(in_progress_rows(&s).is_empty());
    }

    // A log of [Header(foo), Commit(latest reviewable sha)] for the active
    // plan `foo`, used to check WHERE placeholders get spliced.
    fn snap_with_header_and_commit(waiting: WaitingOn) -> StatusSnapshot {
        use crate::cli::log::OnelineRow;
        let mut s = snap(vec![plan_state("foo", waiting)], vec![]);
        let sha = s.plans[0].sha.clone().unwrap();
        s.log_rows = vec![
            OnelineRow::Header {
                plan: Some("foo".into()),
            },
            OnelineRow::Commit {
                sha,
                subject: "do a thing".into(),
                marker: crate::cli::log::RowMarker::Plain,
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
            Seg::InProg(_) => "inprog",
        }
    }

    #[test]
    fn placeholders_inject_under_the_plan_not_at_the_top() {
        // Review placeholder lands in the latest commit's review block:
        // header, THEN the spinner, THEN the commit — under the plan,
        // never floating at index 0.
        let s = snap_with_header_and_commit(reviewer_missing("codex"));
        let ask = block_ask_spans(&s, 80);
        let inp = in_progress_rows(&s);
        let kinds: Vec<&str> = build_scroll(&s, &ask, &inp).iter().map(seg_kind).collect();
        assert_eq!(
            kinds,
            ["header", "inprog", "commit"],
            "review under the plan"
        );

        // Master placeholder lands right after the plan header (its next
        // commit), above the latest commit.
        let s = snap_with_header_and_commit(WaitingOn::MasterToContinue);
        let ask = block_ask_spans(&s, 80);
        let inp = in_progress_rows(&s);
        let kinds: Vec<&str> = build_scroll(&s, &ask, &inp).iter().map(seg_kind).collect();
        assert_eq!(
            kinds,
            ["header", "inprog", "commit"],
            "master under the header"
        );
    }

    #[test]
    fn pending_and_finished_reviews_merge_in_author_order() {
        use crate::cli::log::OnelineRow;
        use clank_core::vocab::Verdict;
        // Active plan waits on reviewer "aaa"; the commit already has a
        // finished review from "zzz". The pending "aaa" must sort BEFORE
        // the done "zzz" (author order) — the exact slot its own finished
        // row would take — so the spinner is replaced in place, not moved.
        let mut s = snap(vec![plan_state("foo", reviewer_missing("aaa"))], vec![]);
        let sha = s.plans[0].sha.clone().unwrap();
        s.log_rows = vec![
            OnelineRow::Header {
                plan: Some("foo".into()),
            },
            OnelineRow::Review {
                verdict: Verdict::Continue,
                author: "zzz".into(),
                summary: "ok".into(),
            },
            OnelineRow::Commit {
                sha,
                subject: "x".into(),
                marker: crate::cli::log::RowMarker::Plain,
            },
        ];
        let ask = block_ask_spans(&s, 80);
        let inp = in_progress_rows(&s);
        let seq = build_scroll(&s, &ask, &inp);
        // header, aaa(pending), zzz(done), commit
        assert!(matches!(&seq[0], Seg::Log(OnelineRow::Header { .. })));
        assert!(
            matches!(&seq[1], Seg::InProg(InProgress::PendingReview { label, .. }) if label == "aaa"),
            "pending aaa sorts before done zzz"
        );
        assert!(matches!(&seq[2], Seg::Log(OnelineRow::Review { author, .. }) if author == "zzz"),);
        assert!(matches!(&seq[3], Seg::Log(OnelineRow::Commit { .. })));
    }

    #[test]
    fn tick_visibility_tracks_scattered_index() {
        // The in-progress row sits at index 1 (after the header), NOT 0.
        let s = snap_with_header_and_commit(WaitingOn::MasterToContinue);
        let ask = block_ask_spans(&s, 80);
        let inp = in_progress_rows(&s);
        let seq = build_scroll(&s, &ask, &inp);
        let visible = |off: usize, cap: usize| {
            seq.iter()
                .enumerate()
                .any(|(i, sg)| matches!(sg, Seg::InProg(_)) && (off..off + cap).contains(&i))
        };
        assert!(visible(0, 3), "in view from the top");
        assert!(
            !visible(5, 2),
            "scrolled past the placeholder → not in view"
        );
    }

    #[test]
    fn entry_overlay_target_resolves_positionally() {
        use crate::cli::log::OnelineRow;
        use crate::lifecycle::CommitSha;
        use clank_core::vocab::Verdict;
        let sha = |s: &str| CommitSha::parse(&format!("{:0<40}", s)).unwrap();
        let commit = |s: &str| OnelineRow::Commit {
            sha: sha(s),
            subject: "x".into(),
            marker: crate::cli::log::RowMarker::Plain,
        };
        let review = |a: &str| OnelineRow::Review {
            verdict: Verdict::Continue,
            author: a.into(),
            summary: "ok".into(),
        };
        let header = || OnelineRow::Header {
            plan: Some("foo".into()),
        };

        // Helpers to read the resolved target.
        let commit_of = |t: Option<OverlayTarget>| match t {
            Some(OverlayTarget::Commit { sha, focus }) => Some((sha, focus)),
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
        assert_eq!(
            commit_of(entry_overlay_target(&seq, 2)),
            Some((sha("c1"), None))
        );
        // A review directly above its commit → that commit, focused on the
        // review's author.
        assert_eq!(
            commit_of(entry_overlay_target(&seq, 1)),
            Some((sha("c1"), Some("aaa".to_string())))
        );
        // A plan header → that plan's document.
        assert!(matches!(
            entry_overlay_target(&seq, 0),
            Some(OverlayTarget::Plan { stem }) if stem == "foo"
        ));
        // A TAIL review with no following commit → None (not a panic, not
        // a bind to some earlier commit).
        assert!(entry_overlay_target(&seq, 4).is_none());
        // Out-of-range cursor → None.
        assert!(entry_overlay_target(&seq, 99).is_none());

        // An ad-hoc header (plan: None) has no document.
        let adhoc = [OnelineRow::Header { plan: None }];
        let seq_adhoc: Vec<Seg> = adhoc.iter().map(Seg::Log).collect();
        assert!(entry_overlay_target(&seq_adhoc, 0).is_none());

        // A stray review whose section ends at a Header before any commit
        // must NOT bind to the NEXT section's commit (the wrong-entry bug).
        let rows2 = [review("zzz"), header(), commit("c2")];
        let seq2: Vec<Seg> = rows2.iter().map(Seg::Log).collect();
        assert!(
            entry_overlay_target(&seq2, 0).is_none(),
            "review before a section break stops at the header"
        );

        // An in-progress placeholder → None.
        let ip = InProgress::MasterWorking {
            name: "m".into(),
            verb: "working",
        };
        let seq3 = vec![Seg::InProg(&ip)];
        assert!(entry_overlay_target(&seq3, 0).is_none());
    }
}
