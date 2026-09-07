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
    PendingReview { label: String, verb: &'static str },
    /// Master is producing the next commit — master's panel row spins.
    /// `verb` says which (working/revising/drafting/finalizing) via
    /// [`verb_of`].
    MasterWorking { name: String, verb: &'static str },
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
        if let [v] = snap.plans.as_slice()
            && let WaitingOn::ReviewerApprovalsMissing { missing }
            | WaitingOn::GateReviewersMissing { missing } = &v.waiting_on
            && missing.iter().any(|l| l.as_str() == label)
        {
            super::derive::verb_of(&v.waiting_on)
        } else {
            "reviewing"
        }
    };
    let mut out: Vec<InProgress> = super::derive::awaited_reviewers(snap)
        .into_iter()
        .map(|label| {
            let verb = plan_verb(&label);
            InProgress::PendingReview { label, verb }
        })
        .collect();
    if let [v] = snap.plans.as_slice() {
        match &v.waiting_on {
            WaitingOn::MasterToContinue
            | WaitingOn::MasterToRevise { .. }
            | WaitingOn::MasterToCommit
            | WaitingOn::MasterToFinalize => out.push(InProgress::MasterWorking {
                name: snap.master.as_deref().unwrap_or("master").to_string(),
                verb: super::derive::verb_of(&v.waiting_on),
            }),
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
        out.push(InProgress::MasterWorking {
            name: snap.master.as_deref().unwrap_or("master").to_string(),
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
        Seg::Log(OnelineRow::Commit { sha, .. })
        | Seg::Log(OnelineRow::PlainCommit { sha, .. }) => Some(OverlayTarget::Commit {
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
        use crate::cli::status_tui::fixtures::with_agents;
        use crate::cli::teams_config::RosterRole;
        let roster: &[(&str, RosterRole)] = &[
            ("claude", RosterRole::Master),
            ("codex", RosterRole::Commit),
        ];
        // Pending reviewers → one spinner row each, verb "reviewing".
        let s = with_agents(
            snap(vec![plan_state("foo", reviewer_missing("codex"))], vec![]),
            roster,
        );
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
    }
}

/// The focused log's guaranteed viewport in a short pane, once the
/// selection has descended far enough (tui-short-pane-whole-scroll).
pub(super) const MIN_LOG_ROWS: usize = 5;

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
///   the full header), capped at [`MIN_LOG_ROWS`], the entries that
///   exist, and what ANY lift can achieve at this height (rows == 1:
///   nothing can — the bar owns the pane, the stated degradation).
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
    let target = (1 + cursor).min(MIN_LOG_ROWS).min(total).min(max_capacity);
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
