//! `clank status --tui` — full-screen, READ-ONLY, live-updating
//! status view sized for a small zellij pane.
//!
//! A third render target over the same machinery `--watch` uses
//! (`StatusSnapshot::build_async` + `build_watcher`/`attach_watcher`)
//! — NOT a new data path. The responsive layout is a PURE function
//! [`render`]`(snapshot, rows, cols) -> Vec<String>` so every sizing
//! behavior is unit-testable headless; only the ioctl, the escape
//! sequences, and the loop touch a real terminal.
//!
//! ## Visual grammar — "instrument panel"
//!
//! One loud signal lamp and a quiet gauge cluster. The top bar
//! (bold + reverse-video, state-colored) carries WHO is active and
//! their verb on the left, the plan stem on the right. Below it, a
//! right-aligned dim label gutter (`gate` / `ask` / `next` /
//! `queue` / `git` / `done`) with each fact stated EXACTLY once —
//! nothing the bar already says is repeated. One hue per frame
//! (the bar's state color); body text is monochrome with dim
//! labels. Small panes get a one-line queue summary (`next … +N`);
//! tall panes expand it to a block.
//!
//! No input handling: no raw mode, no event loop, no TUI framework.
//! Exit is closing the pane (or Ctrl-C — a SIGINT handler restores
//! the terminal first).

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use super::open_zellij::agent_pane_title;
use super::status::{
    StatusSnapshot, short_sha, spawn_sigwinch_forwarder, waiting_actor, watch_status_paths,
};
use clank_core::plan_view::WaitingOn;
use clank_core::wait::PlanWorkState;

// ── span model ──────────────────────────────────────────────
//
// Body lines carry inline styling (dim labels next to plain
// values), and truncation must never split an ANSI escape — so
// lines are built as styled SPANS of plain text, truncated by
// display width at the span level, and only then serialized to
// ANSI.

#[derive(Clone, PartialEq)]
enum Style {
    Plain,
    Dim,
    /// State-colored (the frame's one hue) — used sparingly for
    /// the line that demands action (a pending block's question).
    Accent,
    /// Fixed ANSI color (the log tier's verdict ticks — green ✓ /
    /// cyan ✓✓ / red ✗; lloyd asked for the marks to pop).
    Color(&'static str),
    /// An OSC 8 terminal hyperlink wrapping the visible text; the
    /// String is the (dynamic) target URL — which is why `Style`
    /// isn't `Copy`. `emit` owns the escape, so the width math
    /// counts only the visible text, and the target stays the full
    /// URL even when that text truncates on a narrow pane.
    Link(String),
    /// Background-highlighted (the commit-log plan umbrellas): a
    /// fixed bold + dark-grey-background SGR so a plan name reads as
    /// a section divider (tui-log-plan-highlight-align).
    Highlight,
}

struct Span(Style, String);

fn dim(s: impl Into<String>) -> Span {
    Span(Style::Dim, s.into())
}
fn plain(s: impl Into<String>) -> Span {
    Span(Style::Plain, s.into())
}

/// Right-aligned label gutter: 5 columns + 2 spaces, dim. The
/// fixed gutter is what makes the cluster read as one organized
/// instrument instead of stacked key:value dumps.
fn label(name: &str) -> Span {
    dim(format!("{name:>5}  "))
}

/// The `dirty` gauge: `+12` green, `−3` red (GitHub convention),
/// `· 2 untracked` dim; an all-zero (mode-only) change falls back to
/// a dim `changes`.
///
/// Zero omission is per-PART here — insertions-only renders `+3`,
/// never a red `−0` — which intentionally DIVERGES from the text
/// `dirty_summary`, that omits per-PAIR and shows `+3 −0`. A colored
/// line wants no red zero, and matching GitHub (bare `+3` green) is
/// the point of this gauge. So the two surfaces are NOT mirrors:
/// don't "unify" them by routing the TUI through `dirty_summary` —
/// it would reintroduce the `−0`. Only the `changes` fallback is
/// shared behavior.
fn dirty_spans(d: &super::status::DirtyStats) -> Vec<Span> {
    let mut spans = vec![label("dirty")];
    let mut wrote = false;
    if d.insertions > 0 {
        spans.push(Span(Style::Color("32"), format!("+{}", d.insertions)));
        wrote = true;
    }
    if d.deletions > 0 {
        if wrote {
            spans.push(dim(" "));
        }
        spans.push(Span(Style::Color("31"), format!("−{}", d.deletions)));
        wrote = true;
    }
    if d.untracked > 0 {
        spans.push(dim(if wrote {
            format!(" · {} untracked", d.untracked)
        } else {
            format!("{} untracked", d.untracked)
        }));
        wrote = true;
    }
    if !wrote {
        spans.push(dim("changes"));
    }
    spans
}

// ── pure layout ─────────────────────────────────────────────

/// Render the snapshot into at most `rows` lines, each at most
/// `cols` display columns. Priority-ordered sections, greedy fit.
/// The who's-active bar ALWAYS renders — even at `rows == 1`.
pub(crate) fn render(snap: &StatusSnapshot, rows: u16, cols: u16) -> Vec<String> {
    let rows = rows.max(1) as usize;
    let cols = cols.max(1) as usize;
    let color = state_color(snap);

    let mut out: Vec<String> = Vec::with_capacity(rows);
    out.push(bar(snap, color, cols));

    let mut body: Vec<Vec<Span>> = Vec::new();

    // Breathing room under the bar — the bar needs negative space
    // to read as a lamp, but only once the pane can afford it.
    let breath = rows >= 4;

    // `ask` — a pending block's question is the single case where
    // detail outranks state: a human must act. Word-wrapped (not
    // first-line-truncated) so the reason is fully readable
    // (tui-block-reason-wrap); accent-colored. The label gutter is 7
    // cols (`{:>5}  `), so the text wraps to the remaining width and
    // continuation lines sit under it with a blank gutter.
    let gutter = display_width(&label("ask").1);
    let ask_width = cols.saturating_sub(gutter).max(1);
    for b in snap.blocks.iter().filter(|b| b.answer.is_none()) {
        for (i, line) in wrap(b.question.trim(), ask_width).into_iter().enumerate() {
            body.push(vec![
                label(if i == 0 { "ask" } else { "" }),
                Span(Style::Accent, line),
            ]);
        }
    }

    // `gate` — state + the sha under review. The bar already names
    // the actor, so the waiting-reason is NOT repeated here.
    if let [v] = snap.plans.as_slice() {
        let mut line = vec![label("gate"), plain(v.gate.to_string())];
        if let Some(sha) = &v.sha {
            line.push(dim(format!(" @ {}", short_sha(sha.as_str()))));
        }
        body.push(line);
    }

    // Multi-plan: one aligned line per plan (emoji, actor, plan,
    // gate) — the bar shows only the count.
    if snap.plans.len() > 1 {
        for v in &snap.plans {
            body.push(vec![
                plain(format!("{} ", emoji_of(&v.waiting_on))),
                plain(actor_of(v)),
                dim(format!("  {}  ", v.plan.as_str())),
                plain(v.gate.to_string()),
            ]);
        }
    }

    // Queue: a tall pane gets the full block; otherwise one
    // summary line — head of the queue + how many more.
    let queue_block = rows >= 12 && snap.queue.len() > 1;
    if !snap.queue.is_empty() {
        if queue_block {
            for (i, name) in snap.queue.iter().enumerate() {
                body.push(if i == 0 {
                    vec![label("queue"), plain(name.clone())]
                } else {
                    vec![label(""), dim(name.clone())]
                });
            }
        } else if !(snap.plans.is_empty() && snap.queue.len() == 1) {
            // (suppressed when the bar itself is the promote line
            // for the only queued item — it would be a repeat)
            let mut line = vec![label("next"), plain(snap.queue[0].clone())];
            if snap.queue.len() > 1 {
                line.push(dim(format!(" +{}", snap.queue.len() - 1)));
            }
            body.push(line);
        }
    }

    // `shelf` — shelved plans; the unshelve nudge once a `--for`
    // dependency finishes (plan-lifecycle-verbs).
    for sv in &snap.shelved {
        let note = match (&sv.waiting_for, sv.ready) {
            (Some(w), true) => format!(" — {w} finished; unshelve?"),
            (Some(w), false) => format!(" — waiting on {w}"),
            (None, _) => String::new(),
        };
        body.push(vec![label("shelf"), plain(sv.stem.clone()), dim(note)]);
    }

    // `pr` — active PR review(s): the full GitHub URL on its own
    // line, a clickable OSC 8 hyperlink. The bar carries the
    // actor/verb + `pr #n`; this is the addressable link.
    for pr in &snap.pr_reviews {
        let url = super::status::pr_url(&pr.repo, pr.pr);
        body.push(vec![label("pr"), Span(Style::Link(url.clone()), url)]);
    }

    // `git` — branch + head, dim. When the worktree is dirty the
    // stats get their OWN line below, with GitHub-colored counts
    // (tui-dirty-line-color).
    {
        let branch = snap.branch.as_deref().unwrap_or("?");
        let head = snap.head_sha.as_deref().map(short_sha).unwrap_or("?");
        body.push(vec![label("git"), dim(format!("{branch} {head}"))]);
    }
    if let Some(d) = &snap.dirty {
        body.push(dirty_spans(d));
    }

    // `done` — last finished, only when the repo is idle.
    if snap.plans.is_empty() {
        if let Some(fp) = &snap.last_finished {
            body.push(vec![
                label("done"),
                dim(format!(
                    "{} @ {}",
                    fp.plan.as_str(),
                    short_sha(fp.finalized_at.as_str())
                )),
            ]);
        }
    }

    // Greedy fit: bar (+breath) then body lines until rows run out.
    if breath && out.len() < rows && !body.is_empty() {
        out.push(String::new());
    }
    for line in body {
        if out.len() >= rows {
            break;
        }
        out.push(emit(&line, color, cols));
    }

    // `log` — the LOWEST tier (status-tui-live-log): recent
    // activity fills whatever rows remain, MOST RECENT AT THE TOP
    // (rows arrive newest-first, git-log convention — lloyd), each
    // line styled per row kind + display-width truncated via the
    // same emit path as the gauges. A blank separator when there's
    // room for it plus at least one line.
    if out.len() < rows && !snap.log_rows.is_empty() {
        let mut avail = rows - out.len();
        if avail >= 2 {
            out.push(String::new());
            avail -= 1;
        }
        let shown = &snap.log_rows[..avail.min(snap.log_rows.len())];
        // Align every summary at one column: pad authors to the widest
        // among the rows actually shown.
        let author_width = shown
            .iter()
            .filter_map(|r| match r {
                crate::cli::log::OnelineRow::Review { author, .. } => Some(display_width(author)),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        for row in shown {
            out.push(emit(&log_row_spans(row, author_width), color, cols));
        }
    }
    out
}

/// Widest verdict mark in display columns: `✓✓` (Finished) is 2,
/// the rest are 1. Review marks sit in a field this wide so the
/// author column is constant regardless of verdict.
const MARK_FIELD: usize = 2;

/// Style one log row for the pane. Umbrella headers get a
/// background highlight so they read as section dividers; commit
/// shas are dim at column 2 with plain subjects; review marks
/// (green ✓ / cyan ✓✓ / red ✗) start at that SAME column 2, and the
/// author is padded to `author_width` so every summary begins at one
/// aligned column (tui-log-plan-highlight-align).
fn log_row_spans(row: &crate::cli::log::OnelineRow, author_width: usize) -> Vec<Span> {
    use crate::cli::log::OnelineRow;
    use clank_core::vocab::Verdict;
    match row {
        OnelineRow::Header { plan } => {
            vec![Span(
                Style::Highlight,
                plan.clone().unwrap_or_else(|| "adhoc".to_string()),
            )]
        }
        OnelineRow::Commit { sha, subject } => vec![
            dim(format!("  {} ", &sha.as_str()[..7])),
            plain(subject.clone()),
        ],
        OnelineRow::Review {
            verdict,
            author,
            summary,
        } => {
            let mark_color = match verdict {
                Verdict::Approve => "32",
                Verdict::Finished => "36",
                Verdict::RequestChanges => "31",
                Verdict::Unmarked => "2",
            };
            let mark = crate::cli::log::verdict_mark(*verdict, false);
            // Mark starts at column 2 (== the commit sha); pad it to
            // MARK_FIELD + a separating space so the author column is
            // fixed across verdicts.
            let after_mark = MARK_FIELD.saturating_sub(display_width(&mark)) + 1;
            // Pad the author so the summary column is fixed across
            // reviewers (align the end of the names).
            let author_pad = author_width.saturating_sub(display_width(author));
            let snip = if summary.is_empty() {
                String::new()
            } else {
                format!(": {summary}")
            };
            vec![
                plain("  ".to_string()),
                Span(Style::Color(mark_color), mark),
                plain(" ".repeat(after_mark)),
                dim(format!("{author}{}", " ".repeat(author_pad))),
                plain(snip),
            ]
        }
    }
}

/// The signal lamp: `{emoji} {ACTOR} {verb}` left, plan stem
/// right, gap-filled, bold + reverse-video in the state color,
/// padded to exactly `cols` display columns. The right segment is
/// dropped when the pane is too narrow for both.
fn bar(snap: &StatusSnapshot, color: &str, cols: usize) -> String {
    let (left, right) = bar_text(snap);
    let left = truncate_to(&left, cols);
    let lw = display_width(&left);
    let rw = display_width(&right);
    // Keep the right segment only when it fits with ≥2 cols of gap.
    let body = if !right.is_empty() && lw + 2 + rw <= cols {
        format!("{left}{}{right}", " ".repeat(cols - lw - rw))
    } else {
        format!("{left}{}", " ".repeat(cols - lw))
    };
    format!("\x1b[1;7;{color}m{body}\x1b[0m")
}

/// Bar text: (left = who + verb, right = where). Every state
/// names WHO must act; `idle` only when nothing and nobody waits.
fn bar_text(snap: &StatusSnapshot) -> (String, String) {
    match snap.plans.as_slice() {
        // An active PR review is in-flight work — never idle. Plans
        // take precedence (handled below); among the plan-less
        // states, PR review beats a queue promote.
        [] if !snap.pr_reviews.is_empty() => match snap.pr_reviews.as_slice() {
            [pr] => pr_bar_text(pr, snap.master.as_deref().unwrap_or("master")),
            many => (format!("🔀 {} PR REVIEWS", many.len()), String::new()),
        },
        [] if !snap.queue.is_empty() => {
            let master = snap.master.as_deref().unwrap_or("master").to_uppercase();
            let right = if snap.queue.len() > 1 {
                format!("{} +{}", snap.queue[0], snap.queue.len() - 1)
            } else {
                snap.queue[0].clone()
            };
            (format!("📋 {master} promote"), right)
        }
        [] => ("💤 idle".to_string(), String::new()),
        [v] => (
            format!(
                "{} {} {}",
                emoji_of(&v.waiting_on),
                actor_of(v).to_uppercase(),
                verb_of(&v.waiting_on)
            ),
            v.plan.as_str().to_string(),
        ),
        many => (format!("🔀 {} ACTIVE", many.len()), String::new()),
    }
}

/// PR-review signal-lamp text: `{emoji} {ACTOR} {verb}` + `pr #n`,
/// mirroring the plan bar. Reviewers' turn when any tier member
/// still owes a verdict; otherwise master's turn, by gate.
fn pr_bar_text(pr: &clank_core::wait::PrReviewWorkState, master: &str) -> (String, String) {
    use clank_core::vocab::CommitGateState;
    let right = format!("pr #{}", pr.pr);
    // Round 0: the review isn't open yet — master is drafting the
    // initial comments, then `propose` summons reviewers.
    if pr.round == 0 {
        return (format!("🔨 {} drafting", master.to_uppercase()), right);
    }
    let (emoji, actor, verb) = if let Some(reviewer) = pr.missing_reviewers.first() {
        let emoji = match pr.gate {
            CommitGateState::ApprovedPendingGate => "🔍",
            _ => "👀",
        };
        (emoji, reviewer.as_str().to_string(), "reviewing")
    } else {
        let verb = match pr.gate {
            CommitGateState::Finished => "submitting",
            CommitGateState::ChangesRequested => "integrating",
            _ => "refining",
        };
        let emoji = if pr.gate == CommitGateState::Finished {
            "🏁"
        } else {
            "🔨"
        };
        (emoji, master.to_string(), verb)
    };
    (format!("{emoji} {} {verb}", actor.to_uppercase()), right)
}

/// The bar's leading emoji — the single signal-lamp glyph. Taken
/// from `bar_text` ITSELF (its first token) rather than recomputed,
/// so the zellij tab indicator can never disagree with the bar
/// (tui-tab-mirror-bar-emoji). Every bar left is `"{emoji} …"` and
/// every lamp glyph is a single space-free grapheme, so the first
/// whitespace token is exactly the emoji.
pub(crate) fn bar_emoji(snap: &StatusSnapshot) -> String {
    bar_text(snap)
        .0
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Coarse, human-facing attention state for a worktree — the basis
/// for a zellij tab indicator (spike-zellij-tab-attention):
/// - `Blocked`: a human must act (an unanswered block, or a plan
///   parked on one).
/// - `NeedsCorrection`: HEAD's commit tag doesn't match the plan
///   files it touched (commit-tag-fixup-is-first-class-state) — a
///   self-correctable warning that dominates ordinary work but yields
///   to a human block.
/// - `Idle`: nothing in flight (no active plans, PR reviews, or
///   queued work) — the "asleep" state.
/// - `Active`: anything else (work progressing).
///
/// `state_color` derives ALL its hues from this, so the bar lamp and
/// any tab indicator can never disagree on what "blocked" /
/// "needs-correction" / "idle" means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttentionState {
    Active,
    Idle,
    Blocked,
    NeedsCorrection,
}

pub(crate) fn attention_state(snap: &StatusSnapshot) -> AttentionState {
    let blocked = snap
        .plans
        .iter()
        .any(|v| matches!(v.waiting_on, WaitingOn::Blocked { .. }))
        || snap.blocks.iter().any(|b| b.answer.is_none());
    if blocked {
        return AttentionState::Blocked;
    }
    // Below Blocked, above Active: a broken HEAD tag is a warning the
    // master must self-correct before work resumes.
    let needs_correction = snap
        .plans
        .iter()
        .any(|v| matches!(v.waiting_on, WaitingOn::MasterToFixCommitTag));
    if needs_correction {
        return AttentionState::NeedsCorrection;
    }
    let nothing_in_flight =
        snap.plans.is_empty() && snap.pr_reviews.is_empty() && snap.queue.is_empty();
    if nothing_in_flight {
        AttentionState::Idle
    } else {
        AttentionState::Active
    }
}

/// Whether master is the agent being woken — the SINGLE predicate
/// behind both the bar's green/cyan hue and the per-pane `🔨`
/// (tui-agent-pane-status-emoji). Spelled with `state_color`'s exact
/// BRANCH PRECEDENCE (codex fbdb27b): plans dominate, so a
/// queue-ready / clean-PR state must NOT count as master-active while
/// a plan still awaits reviewers.
pub(crate) fn master_is_active(snap: &StatusSnapshot) -> bool {
    match attention_state(snap) {
        // A broken HEAD tag is master's to fix — master is the active
        // agent (the per-pane 🔨), even though the bar paints orange.
        AttentionState::NeedsCorrection => return true,
        AttentionState::Blocked | AttentionState::Idle => return false,
        AttentionState::Active => {}
    }
    match snap.plans.as_slice() {
        // No plans: PR reviews take precedence over the queue.
        [] if !snap.pr_reviews.is_empty() => {
            // Master's turn iff no PR still owes a reviewer.
            !snap
                .pr_reviews
                .iter()
                .any(|p| !p.missing_reviewers.is_empty())
        }
        [] if !snap.queue.is_empty() => true, // promote
        [] => false,                          // unreachable (Idle covers it)
        // Plans present: queue/PR ignored — master's turn iff some
        // plan is on a master action.
        plans => plans.iter().any(|v| {
            matches!(
                v.waiting_on,
                WaitingOn::MasterToRevise { .. }
                    | WaitingOn::MasterToContinue
                    | WaitingOn::MasterToCommit
                    | WaitingOn::MasterToFinalize
                    | WaitingOn::MasterToFixCommitTag
            )
        }),
    }
}

/// The frame's one hue: red = a human must act (blocked), orange =
/// HEAD tag needs correction, yellow = reviewers, green = master
/// working, cyan = promote, dim idle. The orange is a 256-color SGR
/// (`38;5;208`) — true orange has no 16-color code, and only this
/// branch needs one.
fn state_color(snap: &StatusSnapshot) -> &'static str {
    match attention_state(snap) {
        AttentionState::Blocked => "31",               // red
        AttentionState::NeedsCorrection => "38;5;208", // orange (256-color)
        AttentionState::Idle => "2",                   // dim
        AttentionState::Active => {
            if master_is_active(snap) {
                // cyan for the queue-promote branch (no plans, no
                // PRs), green for master working on a plan or PR.
                if snap.plans.is_empty() && snap.pr_reviews.is_empty() {
                    "36" // cyan: promote
                } else {
                    "32" // green: master
                }
            } else {
                "33" // yellow: reviewers
            }
        }
    }
}

/// Reviewers the team is currently waiting on — the union of every
/// `missing` set (commit + gate tiers across plans, plus each PR
/// review's `missing_reviewers`). May contain duplicates; callers
/// test membership.
pub(crate) fn awaited_reviewers(snap: &StatusSnapshot) -> Vec<&clank_core::ids::AgentLabel> {
    let mut out = Vec::new();
    for p in &snap.plans {
        if let WaitingOn::ReviewerApprovalsMissing { missing }
        | WaitingOn::GateReviewersMissing { missing } = &p.waiting_on
        {
            out.extend(missing.iter());
        }
    }
    for pr in &snap.pr_reviews {
        out.extend(pr.missing_reviewers.iter());
    }
    out
}

/// The status glyph for ONE agent's pane: `🔨` master working / `👀`
/// awaited reviewer / `💤` idle (tui-agent-pane-status-emoji). Coarse
/// by design — every master-work state shows `🔨`; the bar keeps the
/// fine-grained glyph.
fn agent_status_emoji(
    snap: &StatusSnapshot,
    label: &str,
    role: clank_core::vocab::Role,
) -> &'static str {
    match role {
        clank_core::vocab::Role::Master => {
            if master_is_active(snap) {
                "🔨"
            } else {
                "💤"
            }
        }
        clank_core::vocab::Role::Reviewer => {
            if awaited_reviewers(snap).iter().any(|l| l.as_str() == label) {
                "👀"
            } else {
                "💤"
            }
        }
    }
}

fn actor_of(v: &PlanWorkState) -> String {
    match &v.waiting_on {
        // Blocked = awaiting the human, not the block's creator.
        WaitingOn::Blocked { .. } => "human".to_string(),
        w => waiting_actor(w),
    }
}

fn emoji_of(w: &WaitingOn) -> &'static str {
    match w {
        WaitingOn::ReviewerApprovalsMissing { .. } => "👀",
        WaitingOn::GateReviewersMissing { .. } => "🔍",
        WaitingOn::MasterToRevise { .. }
        | WaitingOn::MasterToContinue
        | WaitingOn::MasterToCommit => "🔨",
        WaitingOn::MasterToFinalize => "🏁",
        WaitingOn::MasterToFixCommitTag => "⚠️",
        WaitingOn::Blocked { .. } => "🙋",
    }
}

fn verb_of(w: &WaitingOn) -> &'static str {
    match w {
        WaitingOn::ReviewerApprovalsMissing { .. } => "reviewing",
        WaitingOn::GateReviewersMissing { .. } => "gate-reviewing",
        WaitingOn::MasterToRevise { .. } => "revising",
        WaitingOn::MasterToContinue => "continuing",
        WaitingOn::MasterToCommit => "committing",
        WaitingOn::MasterToFinalize => "finalizing",
        WaitingOn::MasterToFixCommitTag => "fixing tag",
        WaitingOn::Blocked { .. } => "blocked",
    }
}

/// Word-wrap `text` to `width` DISPLAY columns for the `ask` gauge,
/// so a block reason is fully legible instead of first-line-
/// truncated (tui-block-reason-wrap). Display-width aware (emoji = 2,
/// via `char_width`); explicit `\n` are preserved as hard breaks;
/// within a segment it breaks on whitespace, and hard-breaks a
/// single token wider than `width` (a long id/URL can't overflow).
/// `width == 0` degrades to one line per `\n`-segment (no panic).
fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return text.split('\n').map(str::to_string).collect();
    }
    let mut out = Vec::new();
    for segment in text.split('\n') {
        let mut line = String::new();
        let mut line_w = 0usize;
        for word in segment.split_whitespace() {
            let ww = display_width(word);
            let sep = usize::from(!line.is_empty());
            if line_w + sep + ww <= width {
                if sep == 1 {
                    line.push(' ');
                }
                line.push_str(word);
                line_w += sep + ww;
                continue;
            }
            // Doesn't fit: flush the current line first.
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
                line_w = 0;
            }
            if ww <= width {
                line.push_str(word);
                line_w = ww;
            } else {
                // Token wider than the whole line — hard-break it.
                for ch in word.chars() {
                    let cw = char_width(ch);
                    if line_w + cw > width && !line.is_empty() {
                        out.push(std::mem::take(&mut line));
                        line_w = 0;
                    }
                    line.push(ch);
                    line_w += cw;
                }
            }
        }
        // Trailing line; for an empty/whitespace-only segment this
        // preserves the intentional blank line.
        out.push(line);
    }
    out
}

// ── span emission (width math + ANSI) ───────────────────────

/// Serialize spans to one ANSI line, truncated to `cols` display
/// columns with `…`. Truncation happens on the PLAIN text span by
/// span — an escape can never be split, and a dropped span drops
/// its styling with it.
fn emit(spans: &[Span], color: &str, cols: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for Span(style, text) in spans {
        let remaining = cols.saturating_sub(used);
        if remaining == 0 {
            break;
        }
        let piece = truncate_to(text, remaining);
        used += display_width(&piece);
        let truncated_here = piece.ends_with('…') && !text.ends_with('…');
        match style {
            Style::Plain => out.push_str(&piece),
            Style::Dim => out.push_str(&format!("\x1b[2m{piece}\x1b[0m")),
            Style::Accent => out.push_str(&format!("\x1b[{color}m{piece}\x1b[0m")),
            Style::Color(c) => out.push_str(&format!("\x1b[{c}m{piece}\x1b[0m")),
            // OSC 8 hyperlink: ESC ] 8 ; ; <url> ST <text> ESC ] 8 ; ; ST
            Style::Link(url) => out.push_str(&format!("\x1b]8;;{url}\x1b\\{piece}\x1b]8;;\x1b\\")),
            Style::Highlight => out.push_str(&format!("\x1b[1;48;5;238m{piece}\x1b[0m")),
        }
        if truncated_here {
            break;
        }
    }
    out
}

/// Display columns a char occupies in the terminal. Not a full
/// unicode-width implementation: rendered content is validated
/// ASCII (plan stems, agent labels, gate names) plus the fixed
/// status emoji set — so "emoji plane → 2, else 1" is exact for
/// everything we draw, with zero new deps.
fn char_width(c: char) -> usize {
    if ('\u{1F000}'..='\u{1FAFF}').contains(&c) {
        2
    } else {
        1
    }
}

fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Truncate to `cols` DISPLAY columns with a `…` ellipsis.
/// Width-aware (emoji count as 2) so a kept double-width char
/// can't push the line past the pane edge.
fn truncate_to(s: &str, cols: usize) -> String {
    if display_width(s) <= cols {
        return s.to_string();
    }
    if cols == 0 {
        return String::new();
    }
    // Keep chars while they fit in cols-1 (reserving 1 for `…`).
    let mut t = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw > cols - 1 {
            break;
        }
        t.push(c);
        w += cw;
    }
    t.push('…');
    t
}

// ── terminal plumbing (untested by design) ──────────────────

/// (rows, cols) of the stdout tty via `TIOCGWINSZ`. libc carries
/// the correct per-platform request constant + struct layout —
/// hardcoding the number is the portability trap. Falls back to
/// 24x80 when stdout isn't a terminal (piped / headless tests).
pub(crate) fn term_size() -> (u16, u16) {
    use std::os::unix::io::AsRawFd;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let fd = std::io::stdout().as_raw_fd();
    // SAFETY: ws is a valid zeroed winsize; ioctl fills it or
    // returns -1.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
        (ws.ws_row, ws.ws_col)
    } else {
        (24, 80)
    }
}

const ENTER_SEQ: &str = "\x1b[?1049h\x1b[?25l"; // alt-screen + hide cursor
const RESTORE_SEQ: &str = "\x1b[?25h\x1b[?1049l"; // show cursor + leave alt-screen

/// RAII alt-screen guard. `Drop` restores on every normal exit
/// path; a panic hook covers `panic=abort` (where Drop won't run);
/// a SIGINT/SIGTERM handler covers Ctrl-C in a direct terminal
/// (ruthless 9d01e47 concern 3 — without it the user's terminal is
/// left wedged on alt-screen with a hidden cursor).
struct AltScreen;

impl AltScreen {
    fn enter() -> Self {
        print!("{ENTER_SEQ}");
        let _ = std::io::stdout().flush();

        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let mut out = std::io::stdout();
            let _ = out.write_all(RESTORE_SEQ.as_bytes());
            let _ = out.flush();
            prev(info);
        }));

        // SAFETY: installing a handler that only calls
        // async-signal-safe functions (write, _exit).
        let handler =
            restore_and_exit as extern "C" fn(libc::c_int) as *const () as libc::sighandler_t;
        unsafe {
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGTERM, handler);
        }
        AltScreen
    }
}

impl Drop for AltScreen {
    fn drop(&mut self) {
        print!("{RESTORE_SEQ}");
        let _ = std::io::stdout().flush();
    }
}

extern "C" fn restore_and_exit(sig: libc::c_int) {
    const RESTORE: &[u8] = b"\x1b[?25h\x1b[?1049l";
    // SAFETY: write + _exit are async-signal-safe.
    unsafe {
        libc::write(1, RESTORE.as_ptr().cast(), RESTORE.len());
        libc::_exit(128 + sig);
    }
}

/// One frame: cursor home, each line + clear-to-EOL, then clear
/// any leftover rows from a taller previous frame. No full-screen
/// clear → no flicker.
fn paint(lines: &[String]) {
    let mut buf = String::from("\x1b[H");
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            buf.push_str("\r\n");
        }
        buf.push_str(line);
        buf.push_str("\x1b[K");
    }
    buf.push_str("\x1b[J");
    let mut out = std::io::stdout();
    let _ = out.write_all(buf.as_bytes());
    let _ = out.flush();
}

/// The `--tui` loop, fully event-driven: the watcher covers the
/// working tree (gitignore-filtered), `.clank/`, and the git dir;
/// SIGWINCH arrives on the same channel, so a resize is just
/// another wake. Between events there is nothing to redraw —
/// nothing rendered is clock-relative — so the only timeout is a
/// slow backstop against watcher pathologies the error channel
/// doesn't surface (tui-event-driven-dirty-stats).
/// Strip a leading signal-lamp emoji (`"👀 frostsnap"` → `"frostsnap"`)
/// so a prior, un-restored indicator doesn't stack. A lamp glyph is a
/// single emoji-plane grapheme followed by a space.
pub(crate) fn strip_leading_emoji(name: &str) -> String {
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        // A lamp glyph (emoji-plane, width 2) followed by a space.
        (Some(first), Some(' ')) if char_width(first) == 2 => chars.as_str().to_string(),
        _ => name.to_string(),
    }
}

/// The current zellij tab's `(stable id, name)`, or `None` outside
/// zellij or if the query fails. Parses `zellij action
/// current-tab-info` (`id: N` / `name: X` lines).
fn zellij_current_tab() -> Option<(String, String)> {
    std::env::var_os("ZELLIJ")?;
    let out = std::process::Command::new("zellij")
        .args(["action", "current-tab-info"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_current_tab_info(&String::from_utf8_lossy(&out.stdout))
}

/// Parse `zellij action current-tab-info` into `(id, name)`. The
/// output is ONE FIELD PER LINE — verified live against zellij 0.44.3
/// (`od -c`): `name: <name>\nid: <n>\nposition: …`. Order-independent
/// (scans all lines); `None` if either field is absent (don't rename
/// a tab we can't identify). Pure, so the format is unit-tested
/// without spawning zellij (codex 3cc72aa).
fn parse_current_tab_info(stdout: &str) -> Option<(String, String)> {
    let mut id = None;
    let mut name = None;
    for line in stdout.lines() {
        if let Some(v) = line.strip_prefix("id:") {
            id = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("name:") {
            name = Some(v.trim().to_string());
        }
    }
    Some((id?, name?))
}

fn zellij_rename_tab(id: &str, name: &str) {
    // `.output()` (NOT `.status()`): capture + discard the child's
    // stdout/stderr so a rename error never bleeds onto the alt-screen
    // the TUI owns. The loop is event-driven, so an inherited error
    // line would PERSIST until the next watcher event, not flicker
    // (ruthless 9c38523).
    let _ = std::process::Command::new("zellij")
        .args(["action", "rename-tab-by-id", id, name])
        .output();
}

/// Mirrors the bar's emoji into the zellij tab name
/// (tui-tab-mirror-bar-emoji). Captures the tab id + its base name
/// (sans any stale leading glyph) ONCE; renames only on an emoji
/// CHANGE; restores the base name on drop (covers a normal/unwound
/// exit — a signal-killed exit leaves the last glyph, re-synced by the
/// next TUI launch). `None` (no-op) outside zellij. Lifecycle is the
/// pane's: this lives only as long as the `status --tui` process, so
/// there's no separate watcher to leak.
struct TabIndicator {
    id: String,
    base: String,
    last: Option<String>,
}

impl TabIndicator {
    fn new() -> Option<Self> {
        let (id, name) = zellij_current_tab()?;
        Some(Self {
            id,
            base: strip_leading_emoji(&name),
            last: None,
        })
    }

    fn update(&mut self, emoji: &str) {
        if emoji.is_empty() || self.last.as_deref() == Some(emoji) {
            return;
        }
        zellij_rename_tab(&self.id, &format!("{emoji} {}", self.base));
        self.last = Some(emoji.to_string());
    }
}

impl Drop for TabIndicator {
    fn drop(&mut self) {
        if self.last.is_some() {
            zellij_rename_tab(&self.id, &self.base);
        }
    }
}

fn zellij_list_panes() -> Option<String> {
    let out = std::process::Command::new("zellij")
        .args(["action", "list-panes"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn zellij_rename_pane(id: &str, name: &str) {
    // `.output()` (NOT `.status()`): isolate the child's stdout/stderr
    // from the alt-screen — a stale pane id (closed between list-panes
    // and the rename) or any zellij hiccup must not bleed an error line
    // onto the TUI, which the event-driven loop would leave until the
    // next watcher event (ruthless 9c38523).
    let _ = std::process::Command::new("zellij")
        .args(["action", "rename-pane", "--pane-id", id, name])
        .output();
}

/// The agent panes in `zellij action list-panes` output
/// (`PANE_ID  TYPE  TITLE`, one per line) → `(pane_id, label, role)`.
/// A pane is an agent's iff its title (after stripping any leading
/// status glyph) is `"<label> (master)"` / `"<label> (reviewer)"` —
/// the exact format `agent_pane_title` emits. Non-agent panes
/// (status, plugin, the header row) don't match and are skipped.
fn parse_agent_panes(list_panes_stdout: &str) -> Vec<(String, String, clank_core::vocab::Role)> {
    use clank_core::vocab::Role;
    let mut out = Vec::new();
    for line in list_panes_stdout.lines() {
        let mut toks = line.split_whitespace();
        let Some(id) = toks.next() else { continue };
        toks.next(); // TYPE column
        let base = strip_leading_emoji(&toks.collect::<Vec<_>>().join(" "));
        for role in [Role::Master, Role::Reviewer] {
            if let Some(label) = base.strip_suffix(&format!(" ({})", role.as_str())) {
                out.push((id.to_string(), label.to_string(), role));
                break;
            }
        }
    }
    out
}

/// Mirrors each AGENT's status glyph onto its OWN pane name
/// (tui-agent-pane-status-emoji). The `status --tui` pane already
/// holds the whole snapshot and can rename any pane by id, so it owns
/// this centrally — the Stop hook stays clean. Renames a pane only
/// when its desired title CHANGES (dedup per id). `None` (no-op)
/// outside zellij; best-effort.
struct PaneStatus {
    /// Pane id → last title rendered. Dedup: rename a pane only when
    /// its desired title changes.
    last: std::collections::HashMap<String, String>,
    /// Cached `(pane_id, label, role)` map. The pane→agent mapping is
    /// session-stable, so it's fetched via `list-panes` lazily and
    /// reused — NOT re-shelled once per render (status-tui-watch-cpu
    /// Fix 3: the per-render subprocess was loading the zellij server).
    panes: Vec<(String, String, clank_core::vocab::Role)>,
    primed: bool,
    /// Wanted labels we re-queried for and still found no pane — so a
    /// genuinely paneless agent triggers at most one re-query, not one
    /// per render.
    requeried_absent: std::collections::HashSet<String>,
}

impl PaneStatus {
    fn new() -> Option<Self> {
        std::env::var_os("ZELLIJ").map(|_| Self {
            last: std::collections::HashMap::new(),
            panes: Vec::new(),
            primed: false,
            requeried_absent: std::collections::HashSet::new(),
        })
    }

    fn update(&mut self, snap: &StatusSnapshot) {
        self.update_with(snap, zellij_list_panes, zellij_rename_pane);
    }

    /// Core of [`update`] with the zellij I/O injected, so the caching
    /// logic is testable without spawning (no-binary-spawning-tests).
    /// `list_panes` is called only when the cache needs (re)priming;
    /// `rename` only for panes whose title changed.
    fn update_with(
        &mut self,
        snap: &StatusSnapshot,
        mut list_panes: impl FnMut() -> Option<String>,
        mut rename: impl FnMut(&str, &str),
    ) {
        // Refresh the cached pane map only when needed: first run, or
        // when the snapshot wants to mark an agent we have no cached
        // pane for (a pane was likely added). Steady state reuses the
        // cache, so no `list-panes` subprocess fires per render.
        if (!self.primed || self.wants_uncached(snap))
            && let Some(panes) = list_panes()
        {
            self.panes = parse_agent_panes(&panes);
            self.primed = true;
            // A fresh map supersedes the give-up memory; re-record any
            // wanted label that's STILL absent so we don't re-query for
            // it every render.
            self.requeried_absent.clear();
            for label in self.wanted(snap) {
                if !self.has_pane(&label) {
                    self.requeried_absent.insert(label);
                }
            }
        }
        // Build the rename list from the cached map first (immutable
        // borrow), then apply — keeps `self.panes` and `self.last`
        // borrows disjoint.
        let mut renames: Vec<(String, String)> = Vec::new();
        for (id, label, role) in &self.panes {
            let emoji = agent_status_emoji(snap, label, *role);
            let title = format!("{emoji} {}", agent_pane_title(label, role.as_str()));
            if self.last.get(id).map(String::as_str) != Some(title.as_str()) {
                renames.push((id.clone(), title));
            }
        }
        for (id, title) in renames {
            rename(&id, &title);
            self.last.insert(id, title);
        }
    }

    /// Labels the snapshot wants to mark as active: the master and any
    /// awaited reviewers. (Idle agents that already have a cached pane
    /// are handled by the cache; this set only drives the re-query.)
    fn wanted(&self, snap: &StatusSnapshot) -> Vec<String> {
        let mut v: Vec<String> = awaited_reviewers(snap)
            .iter()
            .map(|l| l.as_str().to_string())
            .collect();
        if let Some(m) = snap.master.as_deref() {
            v.push(m.to_string());
        }
        v
    }

    fn has_pane(&self, label: &str) -> bool {
        self.panes.iter().any(|(_, l, _)| l == label)
    }

    /// A wanted agent has no cached pane and we haven't already given
    /// up re-querying for it — a pane likely appeared since we fetched.
    fn wants_uncached(&self, snap: &StatusSnapshot) -> bool {
        self.wanted(snap)
            .into_iter()
            .any(|label| !self.has_pane(&label) && !self.requeried_absent.contains(&label))
    }
}

pub(crate) async fn run_tui(
    repo: PathBuf,
    basename: String,
    home: Option<PathBuf>,
    policy: crate::rebuild::CachePolicy,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<()>();
    let _watcher = watch_status_paths(tx.clone(), &repo)?;
    spawn_sigwinch_forwarder(tx)?;

    let _guard = AltScreen::enter();
    // When inside zellij: mirror the bar's lamp emoji into the tab
    // name, and each agent's status glyph onto its own pane name.
    let mut tab = TabIndicator::new();
    let mut panes = PaneStatus::new();
    loop {
        let snapshot =
            StatusSnapshot::build_async(&repo, &basename, home.as_deref(), policy, None, true)
                .await?;
        let (rows, cols) = term_size();
        paint(&render(&snapshot, rows, cols));
        if let Some(tab) = tab.as_mut() {
            tab.update(&bar_emoji(&snapshot));
        }
        if let Some(panes) = panes.as_mut() {
            panes.update(&snapshot);
        }

        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(()) => while rx.recv_timeout(Duration::from_millis(200)).is_ok() {},
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("filesystem watcher disconnected")
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::lifecycle::{AgentLabel, CommitSha, PlanKey};
    use clank_core::repo_state::NonEmptyVec;
    use clank_core::vocab::CommitGateState;

    pub(crate) fn plan_state(stem: &str, waiting_on: WaitingOn) -> PlanWorkState {
        PlanWorkState {
            plan: PlanKey::parse(stem).unwrap(),
            sha: Some(CommitSha::parse(&format!("{:0<40}", "abc123")).unwrap()),
            gate: CommitGateState::Unreviewed,
            waiting_on,
            touched_code: false,
        }
    }

    pub(crate) fn reviewer_missing(label: &str) -> WaitingOn {
        WaitingOn::ReviewerApprovalsMissing {
            missing: NonEmptyVec::new(vec![AgentLabel::parse(label).unwrap()]).unwrap(),
        }
    }

    pub(crate) fn snap(plans: Vec<PlanWorkState>, queue: Vec<&str>) -> StatusSnapshot {
        StatusSnapshot {
            repo_path: "/r".into(),
            basename: "r".into(),
            branch: Some("master".into()),
            head_sha: Some(format!("{:0<40}", "deadbeef")),
            head_subject: None,
            dirty: None,
            plans,
            last_finished: None,
            blocks: Vec::new(),
            queue: queue.into_iter().map(str::to_string).collect(),
            master: Some("claude".into()),
            shelved: Vec::new(),
            log_rows: Vec::new(),
            pr_reviews: Vec::new(),
        }
    }

    /// Visible text: ANSI escapes removed, trailing pad trimmed.
    /// Tests pin what the EYE sees.
    pub(crate) fn visible(line: &str) -> String {
        visible_untrimmed(line).trim_end().to_string()
    }

    /// `visible` without the trailing-pad trim — for asserting the
    /// bar's exact padded width. Strips both CSI color sequences
    /// (`ESC [ … m`) and OSC 8 hyperlinks (`ESC ] … ST`); the latter
    /// matters because a URL like `github.com` contains an `m`, so
    /// the CSI-only scan would stop mid-URL.
    fn visible_untrimmed(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            match chars.peek() {
                // OSC: ESC ] … terminated by ST (ESC \) or BEL.
                Some(']') => {
                    while let Some(e) = chars.next() {
                        if e == '\x07' {
                            break;
                        }
                        if e == '\x1b' {
                            chars.next(); // consume the ST's `\`
                            break;
                        }
                    }
                }
                // CSI: ESC [ … terminated by a final byte (here `m`).
                _ => {
                    for e in chars.by_ref() {
                        if e == 'm' {
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    #[test]
    fn active_pr_review_is_not_idle() {
        // codex c04c324: a repo with an active PR review and no plans
        // must NOT render idle, in the TUI bar OR `clank status`.
        let mut s = snap(vec![], vec![]);
        s.pr_reviews.push(clank_core::wait::PrReviewWorkState {
            pr: 123,
            repo: "o/r".into(),
            round: 1,
            gate: clank_core::vocab::CommitGateState::Unreviewed,
            missing_reviewers: vec![AgentLabel::parse("codex").unwrap()],
        });
        let bar = visible(&render(&s, 1, 80)[0]);
        assert!(
            bar.contains("CODEX") && bar.contains("pr #123"),
            "bar: {bar}"
        );
        assert!(!bar.contains("idle"), "bar: {bar}");

        let human = s.to_human();
        assert!(human.contains("pr #123"), "to_human: {human}");
        assert!(
            !human.contains("nothing pending"),
            "to_human must not read idle: {human}"
        );
    }

    fn pr_work(round: u64, missing: &[&str]) -> clank_core::wait::PrReviewWorkState {
        clank_core::wait::PrReviewWorkState {
            pr: 5,
            repo: "LLFourn/clank".into(),
            round,
            gate: clank_core::vocab::CommitGateState::Unreviewed,
            missing_reviewers: missing
                .iter()
                .map(|l| AgentLabel::parse(l).unwrap())
                .collect(),
        }
    }

    #[test]
    fn pr_url_renders_as_a_clickable_link_line() {
        let mut s = snap(vec![], vec![]);
        s.pr_reviews.push(pr_work(1, &["codex"]));
        let lines = render(&s, 40, 80);
        let pr = lines
            .iter()
            .find(|l| visible(l).trim_start().starts_with("pr "))
            .expect("a pr url line");
        // Visible text is the bare URL (the `com`/`m` must NOT cut it
        // short — visible() has to skip OSC 8).
        assert_eq!(
            visible(pr).trim_start(),
            "pr  https://github.com/LLFourn/clank/pull/5"
        );
        // The OSC 8 hyperlink wraps it with the full URL as target.
        assert!(
            pr.contains("\x1b]8;;https://github.com/LLFourn/clank/pull/5\x1b\\"),
            "OSC 8 link target: {pr:?}"
        );
    }

    #[test]
    fn round_zero_pr_bar_says_master_drafting_not_reviewing() {
        // The reported bug: at round 0 the bar must show master
        // drafting, NOT a reviewer "reviewing".
        let mut s = snap(vec![], vec![]);
        s.master = Some("claude".into());
        s.pr_reviews.push(pr_work(0, &[]));
        let bar = visible(&render(&s, 1, 80)[0]);
        assert!(
            bar.contains("CLAUDE") && bar.contains("drafting"),
            "bar: {bar}"
        );
        assert!(!bar.contains("reviewing"), "bar: {bar}");
        // And the text surface agrees.
        let human = s.to_human();
        assert!(human.contains("master drafting"), "to_human: {human}");
    }

    #[test]
    fn wrap_breaks_on_words_newlines_and_long_tokens() {
        // Word boundaries.
        assert_eq!(wrap("a b c d", 3), vec!["a b", "c d"]);
        // Explicit newlines preserved as hard breaks.
        assert_eq!(wrap("a\nb", 10), vec!["a", "b"]);
        // Blank line between segments preserved.
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
        // A token wider than the line hard-breaks.
        assert_eq!(wrap("abcdef", 3), vec!["abc", "def"]);
        // Display-width aware: each 🔨 is 2 cols, so two per... no,
        // width 2 fits exactly one per line.
        assert_eq!(wrap("🔨🔨", 2), vec!["🔨", "🔨"]);
        // width 0 degrades to one line per newline-segment, no panic.
        assert_eq!(wrap("a b\nc", 0), vec!["a b", "c"]);
    }

    #[test]
    fn block_reason_wraps_across_rows_within_width() {
        let mut s = snap(vec![], vec![]);
        s.blocks = vec![crate::cli::block::BlockEntry {
            agent: "claude".into(),
            name: "q".into(),
            plan: None,
            question: "the first line is long enough to wrap\nsecond paragraph".into(),
            answer: None,
        }];
        let cols = 24;
        let lines = render(&s, 20, cols as u16);
        let texts: Vec<String> = lines.iter().map(|l| visible(l)).collect();
        // The second paragraph is NOT dropped (the bug being fixed).
        assert!(
            texts.iter().any(|t| t.contains("second paragraph")),
            "continuation must survive: {texts:?}"
        );
        // The long first line wrapped onto multiple ask rows (the
        // first carries the `ask` gutter; continuations are indented).
        let ask_rows = texts.iter().filter(|t| t.contains("first")).count()
            + texts.iter().filter(|t| t.contains("wrap")).count();
        assert!(ask_rows >= 1, "first line present: {texts:?}");
        // Nothing exceeds the pane width.
        for line in &lines {
            assert!(
                display_width(visible(line).trim_end()) <= cols,
                "line wider than {cols}: `{line}`"
            );
        }
    }

    #[test]
    fn bar_emoji_is_the_bars_leading_glyph() {
        // Single source: bar_emoji == the first token of the bar's
        // left segment, for every representative state.
        let cases = vec![
            snap(vec![], vec![]),                                             // idle
            snap(vec![plan_state("foo", reviewer_missing("codex"))], vec![]), // reviewers
            snap(vec![], vec!["queued"]),                                     // promote
        ];
        for s in &cases {
            let (left, _) = bar_text(s);
            let want = left.split_whitespace().next().unwrap();
            assert_eq!(bar_emoji(s), want, "bar_emoji must match the bar: {left:?}");
        }
        // Idle is the sleeping glyph specifically.
        assert_eq!(bar_emoji(&snap(vec![], vec![])), "💤");
    }

    #[test]
    fn parse_current_tab_info_matches_zellij_044_output() {
        // The EXACT bytes from `zellij action current-tab-info` on
        // zellij 0.44.3 (verified via `od -c`): one field per line.
        let out = "name: clank\nid: 0\nposition: 0\n";
        assert_eq!(
            parse_current_tab_info(out),
            Some(("0".to_string(), "clank".to_string()))
        );
        // Order-independent.
        let reordered = "id: 3\nname: my tab\n";
        assert_eq!(
            parse_current_tab_info(reordered),
            Some(("3".to_string(), "my tab".to_string()))
        );
        // Missing id (or name) → None: never rename an unidentified tab.
        assert_eq!(parse_current_tab_info("position: 0\n"), None);
    }

    #[test]
    fn strip_leading_emoji_removes_a_stale_glyph_only() {
        // A prior, un-restored indicator must not stack.
        assert_eq!(strip_leading_emoji("👀 frostsnap"), "frostsnap");
        assert_eq!(strip_leading_emoji("💤 clank"), "clank");
        // A plain name is untouched.
        assert_eq!(strip_leading_emoji("clank"), "clank");
        // A name that merely starts with a word (no emoji) is untouched.
        assert_eq!(strip_leading_emoji("pr-497"), "pr-497");
    }

    fn pr_awaiting(missing: &[&str]) -> clank_core::wait::PrReviewWorkState {
        clank_core::wait::PrReviewWorkState {
            pr: 1,
            repo: "o/r".into(),
            round: 1,
            gate: clank_core::vocab::CommitGateState::Unreviewed,
            missing_reviewers: missing
                .iter()
                .map(|l| AgentLabel::parse(l).unwrap())
                .collect(),
        }
    }

    #[test]
    fn master_is_active_follows_state_color_precedence() {
        // Idle → false.
        assert!(!master_is_active(&snap(vec![], vec![])));
        // Plan on a master action → true.
        assert!(master_is_active(&snap(
            vec![plan_state("p", WaitingOn::MasterToContinue)],
            vec![]
        )));
        // THE codex fbdb27b case: a plan awaiting reviewers dominates a
        // pending queue → master is NOT active (bar stays yellow).
        assert!(
            !master_is_active(&snap(
                vec![plan_state("p", reviewer_missing("codex"))],
                vec!["queued"]
            )),
            "plans take precedence over the queue"
        );
        // No plans + queue only → master promotes.
        assert!(master_is_active(&snap(vec![], vec!["queued"])));
        // No plans + PR: master's turn iff no reviewer is owed.
        let mut s = snap(vec![], vec![]);
        s.pr_reviews.push(pr_awaiting(&["codex"]));
        assert!(!master_is_active(&s), "PR still owes a reviewer");
        s.pr_reviews[0].missing_reviewers.clear();
        assert!(master_is_active(&s), "PR fully reviewed → master's turn");
    }

    #[test]
    fn awaited_reviewers_unions_plans_and_prs() {
        let mut s = snap(vec![plan_state("p", reviewer_missing("codex"))], vec![]);
        s.pr_reviews.push(pr_awaiting(&["ruthless"]));
        let got: Vec<&str> = awaited_reviewers(&s).iter().map(|l| l.as_str()).collect();
        assert!(
            got.contains(&"codex") && got.contains(&"ruthless"),
            "got: {got:?}"
        );
        // Master's turn → nobody awaited.
        assert!(
            awaited_reviewers(&snap(
                vec![plan_state("p", WaitingOn::MasterToContinue)],
                vec![]
            ))
            .is_empty()
        );
    }

    #[test]
    fn agent_status_emoji_per_role() {
        use clank_core::vocab::Role;
        let working = snap(vec![plan_state("p", WaitingOn::MasterToContinue)], vec![]);
        assert_eq!(agent_status_emoji(&working, "claude", Role::Master), "🔨");
        assert_eq!(agent_status_emoji(&working, "codex", Role::Reviewer), "💤");

        let reviewing = snap(vec![plan_state("p", reviewer_missing("codex"))], vec![]);
        assert_eq!(
            agent_status_emoji(&reviewing, "codex", Role::Reviewer),
            "👀"
        );
        assert_eq!(
            agent_status_emoji(&reviewing, "ruthless", Role::Reviewer),
            "💤"
        );
        assert_eq!(agent_status_emoji(&reviewing, "claude", Role::Master), "💤");

        assert_eq!(
            agent_status_emoji(&snap(vec![], vec![]), "claude", Role::Master),
            "💤"
        );
    }

    #[test]
    fn parse_agent_panes_from_live_list_panes() {
        use clank_core::vocab::Role;
        // Exact `zellij action list-panes` shape (0.44.3); terminal_1
        // carries a stale glyph that must be stripped.
        let out = "\
PANE_ID  TYPE  TITLE
plugin_0  plugin  (.) - zellij:link
terminal_0  terminal  claude (master)
terminal_1  terminal  👀 codex (reviewer)
terminal_2  terminal  status
terminal_3  terminal  ruthless (reviewer)
";
        assert_eq!(
            parse_agent_panes(out),
            vec![
                ("terminal_0".to_string(), "claude".to_string(), Role::Master),
                (
                    "terminal_1".to_string(),
                    "codex".to_string(),
                    Role::Reviewer
                ),
                (
                    "terminal_3".to_string(),
                    "ruthless".to_string(),
                    Role::Reviewer
                ),
            ]
        );
    }

    #[test]
    fn pane_status_caches_list_panes_and_renames_only_on_change() {
        // Fix 3 (status-tui-watch-cpu): the pane→agent map is fetched
        // ONCE and reused — no `list-panes` subprocess per render — and
        // a pane is renamed only when its emoji actually changes.
        use std::cell::{Cell, RefCell};

        let panes_out = "0 PANE claude (master)\n1 PANE codex (reviewer)\n";
        let list_calls = Cell::new(0usize);
        let renames = RefCell::new(Vec::<(String, String)>::new());
        let mut ps = PaneStatus {
            last: std::collections::HashMap::new(),
            panes: Vec::new(),
            primed: false,
            requeried_absent: std::collections::HashSet::new(),
        };
        let go = |ps: &mut PaneStatus, s: &StatusSnapshot| {
            ps.update_with(
                s,
                || {
                    list_calls.set(list_calls.get() + 1);
                    Some(panes_out.to_string())
                },
                |id, title| {
                    renames
                        .borrow_mut()
                        .push((id.to_string(), title.to_string()))
                },
            );
        };

        // First render: one `list-panes`; both agent panes get a title.
        let awaited = snap(vec![plan_state("p", reviewer_missing("codex"))], vec![]);
        go(&mut ps, &awaited);
        assert_eq!(list_calls.get(), 1, "primed with one list-panes");
        let after_first = renames.borrow().len();
        assert_eq!(after_first, 2, "both panes renamed on first render");

        // Same snapshot, many renders: NO further list-panes, NO renames.
        for _ in 0..5 {
            go(&mut ps, &awaited);
        }
        assert_eq!(list_calls.get(), 1, "list-panes reused from cache");
        assert_eq!(
            renames.borrow().len(),
            after_first,
            "unchanged emoji => no rename"
        );

        // Master's turn instead: the plan's wait state toggles BOTH
        // emojis at once — master 💤→🔨 and codex 👀→💤 — so two panes
        // rename. Still no re-query: both panes are cached.
        let idle = snap(vec![plan_state("p", WaitingOn::MasterToContinue)], vec![]);
        go(&mut ps, &idle);
        assert_eq!(list_calls.get(), 1, "no re-query: panes already cached");
        assert_eq!(
            renames.borrow().len(),
            after_first + 2,
            "both flipped panes renamed"
        );
    }

    #[test]
    fn agent_pane_title_round_trips_through_parse() {
        use clank_core::vocab::Role;
        // The shared builder's output is recoverable by the parser —
        // pins layout + renamer to one format (no silent drift).
        for (role, label) in [(Role::Master, "alice"), (Role::Reviewer, "bob")] {
            let line = format!(
                "terminal_9  terminal  {}",
                agent_pane_title(label, role.as_str())
            );
            assert_eq!(
                parse_agent_panes(&line),
                vec![("terminal_9".to_string(), label.to_string(), role)]
            );
        }
    }

    #[test]
    fn attention_state_classifies_blocked_idle_active() {
        // Idle: nothing in flight.
        assert_eq!(attention_state(&snap(vec![], vec![])), AttentionState::Idle);

        // Active: an in-flight plan, OR queued work, OR a PR review.
        assert_eq!(
            attention_state(&snap(
                vec![plan_state("foo", reviewer_missing("codex"))],
                vec![]
            )),
            AttentionState::Active
        );
        assert_eq!(
            attention_state(&snap(vec![], vec!["queued"])),
            AttentionState::Active,
            "queued work is not idle — master owes a promote"
        );
        let mut s = snap(vec![], vec![]);
        s.pr_reviews.push(clank_core::wait::PrReviewWorkState {
            pr: 1,
            repo: "o/r".into(),
            round: 1,
            gate: clank_core::vocab::CommitGateState::Unreviewed,
            missing_reviewers: vec![AgentLabel::parse("codex").unwrap()],
        });
        assert_eq!(attention_state(&s), AttentionState::Active);

        // Blocked: an unanswered block dominates even with no plans.
        let mut s = snap(vec![], vec!["queued"]);
        s.blocks = vec![crate::cli::block::BlockEntry {
            agent: "claude".into(),
            name: "q".into(),
            plan: None,
            question: "halt?".into(),
            answer: None,
        }];
        assert_eq!(
            attention_state(&s),
            AttentionState::Blocked,
            "an unanswered block outranks active work"
        );
    }

    #[test]
    fn needs_correction_is_orange_above_active_below_blocked() {
        // commit-tag-fixup-is-first-class-state: a MasterToFixCommitTag
        // plan row → NeedsCorrection (orange), above ordinary Active.
        let s = snap(
            vec![plan_state("foo", WaitingOn::MasterToFixCommitTag)],
            vec![],
        );
        assert_eq!(attention_state(&s), AttentionState::NeedsCorrection);
        assert_eq!(state_color(&s), "38;5;208", "orange 256-color SGR");
        // Master is the actor (per-pane 🔨), and the bar emoji is ⚠️ —
        // bar lamp + tab indicator both derived from attention_state,
        // so they can't disagree.
        assert!(master_is_active(&s));
        assert_eq!(emoji_of(&WaitingOn::MasterToFixCommitTag), "⚠️");

        // ...but a human block still outranks the correction.
        let mut blocked = s;
        blocked.blocks = vec![crate::cli::block::BlockEntry {
            agent: "claude".into(),
            name: "q".into(),
            plan: None,
            question: "halt?".into(),
            answer: None,
        }];
        assert_eq!(
            attention_state(&blocked),
            AttentionState::Blocked,
            "a human block outranks the tag correction"
        );
    }

    #[test]
    fn dirty_stats_get_their_own_github_colored_line() {
        use crate::cli::status::DirtyStats;
        let mut s = snap(vec![], vec![]);
        s.dirty = Some(DirtyStats {
            insertions: 160,
            deletions: 45,
            untracked: 2,
        });
        let lines = render(&s, 40, 80);

        // The git line carries branch+head only — no dirty stats.
        let git = lines
            .iter()
            .find(|l| visible(l).trim_start().starts_with("git"))
            .unwrap();
        assert!(!visible(git).contains("160"), "git line: {}", visible(git));

        // A distinct `dirty` gauge line holds the stats.
        let dirty = lines
            .iter()
            .find(|l| visible(l).starts_with("dirty"))
            .expect("a dirty line");
        assert_eq!(visible(dirty), "dirty  +160 −45 · 2 untracked");
        // GitHub colors: green wraps the additions, red the deletions.
        assert!(dirty.contains("\x1b[32m+160"), "green additions: {dirty:?}");
        assert!(dirty.contains("\x1b[31m−45"), "red deletions: {dirty:?}");
    }

    #[test]
    fn dirty_line_omits_zeros_and_falls_back_to_changes() {
        use crate::cli::status::DirtyStats;
        let mut s = snap(vec![], vec![]);

        // Only insertions → no `−0`, no red.
        s.dirty = Some(DirtyStats {
            insertions: 3,
            deletions: 0,
            untracked: 0,
        });
        let dirty = render(&s, 40, 80)
            .into_iter()
            .find(|l| visible(l).starts_with("dirty"))
            .unwrap();
        assert_eq!(visible(&dirty), "dirty  +3");
        assert!(!dirty.contains("\x1b[31m"), "no red without deletions");

        // All-zero (e.g. mode-only change) → dim `changes`, no color.
        s.dirty = Some(DirtyStats {
            insertions: 0,
            deletions: 0,
            untracked: 0,
        });
        let dirty = render(&s, 40, 80)
            .into_iter()
            .find(|l| visible(l).starts_with("dirty"))
            .unwrap();
        assert_eq!(visible(&dirty), "dirty  changes");
        assert!(!dirty.contains("\x1b[32m") && !dirty.contains("\x1b[31m"));
    }

    #[test]
    fn clean_tree_emits_no_dirty_line() {
        let s = snap(vec![], vec![]); // dirty: None
        let lines = render(&s, 40, 80);
        assert!(
            !lines.iter().any(|l| visible(l).starts_with("dirty")),
            "clean tree must not render a dirty line"
        );
    }

    #[test]
    fn one_row_always_renders_active_agent_bar() {
        // THE invariant: even at 1 row the who's-active bar renders
        // (ruthless 9d01e47 concern 1) — actor + verb left, plan
        // right.
        let s = snap(vec![plan_state("foo", reviewer_missing("codex"))], vec![]);
        let lines = render(&s, 1, 40);
        assert_eq!(lines.len(), 1);
        let v = visible(&lines[0]);
        assert!(v.starts_with("👀 CODEX reviewing"), "got `{v}`");
        assert!(v.ends_with("foo"), "plan stem right-aligned; got `{v}`");
    }

    #[test]
    fn bar_drops_right_segment_when_too_narrow() {
        let s = snap(
            vec![plan_state(
                "a-very-long-plan-name-that-will-not-fit",
                reviewer_missing("codex"),
            )],
            vec![],
        );
        let v = visible(&render(&s, 1, 20)[0]);
        assert_eq!(v, "👀 CODEX reviewing", "left segment only; got `{v}`");
    }

    #[test]
    fn idle_renders_idle_even_at_one_row() {
        // Truly idle: no plans AND empty queue.
        let s = snap(vec![], vec![]);
        assert_eq!(visible(&render(&s, 1, 80)[0]), "💤 idle");
    }

    #[test]
    fn no_plan_with_queue_names_master_and_next_promote() {
        // lloyd (reopen round): no active plan + non-empty queue is
        // MASTER's turn — promote. Names the agent; queue head on
        // the right with the remainder count.
        let s = snap(vec![], vec!["zellij-layout", "wfw-hint"]);
        let v = visible(&render(&s, 1, 60)[0]);
        assert!(v.starts_with("📋 CLAUDE promote"), "got `{v}`");
        assert!(v.ends_with("zellij-layout +1"), "got `{v}`");
    }

    #[test]
    fn no_plan_with_queue_and_no_team_falls_back_to_master() {
        let mut s = snap(vec![], vec!["zellij-layout"]);
        s.master = None;
        let v = visible(&render(&s, 1, 60)[0]);
        assert!(v.starts_with("📋 MASTER promote"), "got `{v}`");
    }

    #[test]
    fn single_plan_body_states_each_fact_once() {
        // The bar names actor+verb+plan; the body must NOT repeat
        // them — gate line carries state + sha, git line the repo.
        let s = snap(
            vec![plan_state("foo", reviewer_missing("codex"))],
            vec!["q1"],
        );
        let lines = render(&s, 8, 60);
        let texts: Vec<String> = lines.iter().map(|l| visible(l)).collect();
        assert_eq!(texts[1], "", "breath line under the bar");
        assert_eq!(texts[2], " gate  unreviewed @ abc1230");
        assert_eq!(texts[3], " next  q1");
        assert_eq!(texts[4], "  git  master deadbee");
        // No body line repeats the actor or the plan stem.
        for t in &texts[1..] {
            assert!(!t.contains("codex") && !t.contains("foo"), "repeat: `{t}`");
        }
    }

    #[test]
    fn width_truncates_every_line_by_display_width() {
        let s = snap(
            vec![plan_state(
                "a-very-long-plan-name-that-will-not-fit",
                reviewer_missing("codex"),
            )],
            vec!["another-quite-long-queued-name"],
        );
        let lines = render(&s, 24, 10);
        for line in &lines {
            assert!(
                display_width(visible(line).trim_end()) <= 10,
                "line wider than 10 display cols: `{line}`"
            );
        }
    }

    #[test]
    fn bar_padding_fills_exactly_to_display_width() {
        // The reverse-video bar must be EXACTLY cols display-wide —
        // one more wraps the background (the bug lloyd hit live).
        let s = snap(vec![plan_state("foo", reviewer_missing("codex"))], vec![]);
        for cols in [9, 20, 39, 60] {
            let lines = render(&s, 1, cols);
            assert_eq!(
                display_width(&visible_untrimmed(&lines[0])),
                cols as usize,
                "bar must be exactly {cols} display cols"
            );
        }
    }

    #[test]
    fn tiny_pane_keeps_bar_and_gate_before_queue() {
        // Greedy order: bar, gate; the queue summary only once
        // there's room (no breath line below 4 rows).
        let s = snap(
            vec![plan_state("foo", reviewer_missing("codex"))],
            vec!["q1", "q2"],
        );
        let texts: Vec<String> = render(&s, 2, 60).iter().map(|l| visible(l)).collect();
        assert!(texts[1].starts_with(" gate"), "got {texts:?}");
        let texts: Vec<String> = render(&s, 3, 60).iter().map(|l| visible(l)).collect();
        assert!(texts[2].starts_with(" next  q1 +1"), "got {texts:?}");
    }

    #[test]
    fn tall_pane_expands_queue_block() {
        let s = snap(
            vec![plan_state("foo", reviewer_missing("codex"))],
            vec!["first", "second", "third"],
        );
        let texts: Vec<String> = render(&s, 14, 60).iter().map(|l| visible(l)).collect();
        let qi = texts
            .iter()
            .position(|t| t.starts_with("queue  first"))
            .expect("queue block header");
        assert_eq!(texts[qi + 1].trim(), "second");
        assert_eq!(texts[qi + 2].trim(), "third");
        assert!(
            !texts.iter().any(|t| t.contains("next")),
            "summary line replaced by block"
        );
    }

    #[test]
    fn blocked_plan_bar_names_human_and_ask_carries_question() {
        use clank_core::plan_view::PlanBlock;
        let mut s = snap(
            vec![plan_state(
                "foo",
                WaitingOn::Blocked {
                    block: PlanBlock {
                        creator: AgentLabel::parse("claude").unwrap(),
                        name: "q".into(),
                        message: "is this right?\nmore detail".into(),
                    },
                },
            )],
            vec![],
        );
        s.blocks = vec![crate::cli::block::BlockEntry {
            agent: "claude".into(),
            name: "q".into(),
            plan: None,
            question: "is this right?\nmore detail".into(),
            answer: None,
        }];
        let texts: Vec<String> = render(&s, 6, 60).iter().map(|l| visible(l)).collect();
        assert!(texts[0].starts_with("🙋 HUMAN blocked"), "got {texts:?}");
        // The reason WORD-WRAPS now — the second line ("more detail")
        // is no longer dropped, and continuation aligns under the text
        // with a blank gutter (tui-block-reason-wrap).
        assert_eq!(texts[2], "  ask  is this right?");
        assert_eq!(texts[3], "       more detail");
    }

    #[test]
    fn two_plans_get_count_bar_and_per_plan_lines() {
        let s = snap(
            vec![
                plan_state("alpha", reviewer_missing("codex")),
                plan_state("beta", WaitingOn::MasterToContinue),
            ],
            vec![],
        );
        let texts: Vec<String> = render(&s, 8, 60).iter().map(|l| visible(l)).collect();
        assert!(texts[0].starts_with("🔀 2 ACTIVE"), "got {texts:?}");
        assert_eq!(texts[2], "👀 codex  alpha  unreviewed");
        assert_eq!(texts[3], "🔨 master  beta  unreviewed");
    }

    #[test]
    fn idle_with_done_shows_last_finished() {
        use clank_core::repo_state::FinishedPlan;
        let mut s = snap(vec![], vec![]);
        s.last_finished = Some(FinishedPlan {
            plan: PlanKey::parse("old-plan").unwrap(),
            intro: CommitSha::parse(&format!("{:0<40}", "aa")).unwrap(),
            finalized_at: CommitSha::parse(&format!("{:0<40}", "bb")).unwrap(),
        });
        let texts: Vec<String> = render(&s, 6, 60).iter().map(|l| visible(l)).collect();
        assert_eq!(texts[0], "💤 idle");
        assert!(
            texts.iter().any(|t| t.starts_with(" done  old-plan @ ")),
            "got {texts:?}"
        );
    }
}

#[cfg(test)]
mod log_tier_tests {
    use super::tests::{plan_state, reviewer_missing, snap, visible};
    use super::*;

    fn commit_row(subject: &str) -> crate::cli::log::OnelineRow {
        crate::cli::log::OnelineRow::Commit {
            sha: crate::lifecycle::CommitSha::parse(&format!("{:0<40}", "abc1234")).unwrap(),
            subject: subject.to_string(),
        }
    }

    fn snap_with_log(subjects: &[&str]) -> StatusSnapshot {
        let mut s = snap(vec![plan_state("foo", reviewer_missing("codex"))], vec![]);
        s.log_rows = subjects.iter().map(|l| commit_row(l)).collect();
        s
    }

    #[test]
    fn log_fills_leftover_rows_most_recent_at_top() {
        // 8 rows: bar + breath + gate + git = 4, separator + 3 log
        // lines fit → the TAIL of the log (most recent) is shown.
        // Rows arrive newest-first (e5 is the most recent commit).
        let s = snap_with_log(&["e5", "e4", "e3", "e2", "e1"]);
        let texts: Vec<String> = render(&s, 8, 60).iter().map(|l| visible(l)).collect();
        assert_eq!(texts.len(), 8);
        assert!(
            texts[5].ends_with("e5"),
            "most recent at the TOP of the log: {texts:?}"
        );
        assert!(texts[6].ends_with("e4"), "got {texts:?}");
        assert!(texts[7].ends_with("e3"), "got {texts:?}");
        assert!(
            !texts.iter().any(|t| t.ends_with("e1")),
            "oldest dropped first"
        );
    }

    #[test]
    fn log_absent_when_no_rows_remain() {
        let s = snap_with_log(&["e1", "e2"]);
        // 3 rows: bar + gate + git eat everything.
        let texts: Vec<String> = render(&s, 3, 60).iter().map(|l| visible(l)).collect();
        assert!(
            !texts.iter().any(|t| t.ends_with("e1") || t.ends_with("e2")),
            "no log rows at 3 rows: {texts:?}"
        );
    }

    #[test]
    fn log_lines_truncate_by_display_width() {
        // Wide-char commit subjects (emoji) must truncate by
        // display columns — the same blind spot the banner had
        // (ruthless 54c37f6 concern 3).
        let s = snap_with_log(&["🔨🔨🔨🔨🔨🔨 a very wide subject"]);
        let lines = render(&s, 10, 14);
        for line in &lines {
            assert!(
                display_width(visible(line).trim_end()) <= 14,
                "log line wider than 14 display cols: `{line}`"
            );
        }
    }

    #[test]
    fn log_rows_styled_per_kind() {
        use crate::cli::log::OnelineRow;
        use clank_core::vocab::Verdict;
        let mut s = snap(vec![plan_state("foo", reviewer_missing("codex"))], vec![]);
        s.log_rows = vec![
            OnelineRow::Header {
                plan: Some("foo".into()),
            },
            commit_row("intro"),
            OnelineRow::Review {
                verdict: Verdict::Approve,
                author: "codex".into(),
                summary: "lgtm".into(),
            },
        ];
        let lines = render(&s, 10, 60);
        let texts: Vec<String> = lines.iter().map(|l| visible(l)).collect();
        // Umbrella header (visible text is the bare plan name) and the
        // commit indented under it at column 2.
        assert!(texts.iter().any(|t| t == "foo"), "header: {texts:?}");
        assert!(
            texts.iter().any(|t| t.starts_with("  abc1234 intro")),
            "commit indented: {texts:?}"
        );
        let raw = lines.join("");
        // The header carries the background-highlight SGR.
        assert!(
            raw.contains("\x1b[1;48;5;238mfoo\x1b[0m"),
            "header background-highlighted: {raw:?}"
        );
        // The verdict tick is COLORED (green for approve) in the raw
        // ANSI output — lloyd's "make the ticks pop".
        assert!(
            raw.contains("\x1b[32m✓\x1b[0m"),
            "approve tick must be green: {raw:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("✓  codex: lgtm")),
            "review line: {texts:?}"
        );
    }

    #[test]
    fn review_marks_align_with_sha_and_summaries_align() {
        use crate::cli::log::OnelineRow;
        use clank_core::vocab::Verdict;
        let review = |v, author: &str| OnelineRow::Review {
            verdict: v,
            author: author.into(),
            summary: "why".into(),
        };
        let mut s = snap(vec![plan_state("foo", reviewer_missing("codex"))], vec![]);
        s.log_rows = vec![
            commit_row("intro"),
            review(Verdict::Approve, "codex"), // ✓  (1-wide mark, mid name)
            review(Verdict::Finished, "ruthless"), // ✓✓ (2-wide mark, long name)
            review(Verdict::RequestChanges, "zz"), // ✗  (1-wide mark, short name)
        ];
        let texts: Vec<String> = render(&s, 12, 80).iter().map(|l| visible(l)).collect();

        // Display COLUMN (not byte offset — ✓ is 3 bytes/1 col, ✓✓ is
        // 6 bytes/2 cols) of where `needle` begins on a line.
        let col = |line: &str, needle: &str| display_width(&line[..line.find(needle).unwrap()]);

        // The commit sha sits at column 2 (after "  ").
        let commit = texts.iter().find(|t| t.contains("abc1234")).unwrap();
        assert_eq!(col(commit, "abc1234"), 2, "sha at column 2: {commit:?}");

        // Every review mark starts at that SAME column 2, and every
        // summary (`: why`) starts at one shared column regardless of
        // mark width or author length.
        let mut summary_cols = Vec::new();
        for (mark, name) in [("✓", "codex"), ("✓✓", "ruthless"), ("✗", "zz")] {
            let line = texts.iter().find(|t| t.contains(name)).unwrap();
            assert_eq!(col(line, mark), 2, "mark at sha column: {line:?}");
            summary_cols.push(col(line, ": why"));
        }
        assert!(
            summary_cols.iter().all(|c| *c == summary_cols[0]),
            "summaries align across marks + name lengths: {summary_cols:?}"
        );
    }
}
