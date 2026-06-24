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
use std::sync::atomic::{AtomicPtr, Ordering};
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
    /// Italic — the master "working…" in-progress row
    /// (status-timeline-progress).
    Italic,
}

#[derive(Clone)]
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

/// The roster tier as shown in the UI: `master` / `commit` / `gate`.
fn tier_label(role: crate::cli::teams_config::RosterRole) -> &'static str {
    use crate::cli::teams_config::RosterRole;
    match role {
        RosterRole::Master => "master",
        RosterRole::Commit => "commit",
        RosterRole::Gate => "gate",
    }
}

/// The armed auto-mode mark in a FIXED [`MARK_FIELD`]-wide field —
/// `▶` (playing, green) when auto runs the agent's loop, `⏸` (paused,
/// dim) when parked. Padding the glyph into a fixed field (not relying
/// on the two glyphs happening to share a width) is what keeps the
/// name column from jittering between on/off rows.
fn auto_mark(mode: clank_core::vocab::AutoMode) -> Span {
    use clank_core::vocab::AutoMode;
    let (style, glyph) = match mode {
        AutoMode::On => (Style::Color("32"), "▶"),
        AutoMode::Off => (Style::Dim, "⏸"),
    };
    let pad = MARK_FIELD.saturating_sub(display_width(glyph));
    Span(style, format!("{glyph}{}", " ".repeat(pad)))
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
fn dirty_spans(d: &crate::git_io::DirtyStats) -> Vec<Span> {
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

/// Render the snapshot into at most `rows` lines, each at most `cols`
/// display columns. Priority-ordered sections, greedy fit. The
/// who's-active bar ALWAYS renders — even at `rows == 1`.
///
/// The log tier is scrolled down by `offset` rows (0 = newest at top,
/// the live default). Returns the painted lines AND the log viewport
/// capacity (how many log rows fit) so the caller can clamp the offset
/// and know when to page in older rows.
pub(crate) fn render_at(
    snap: &StatusSnapshot,
    rows: u16,
    cols: u16,
    offset: usize,
    frame: usize,
    view: &PanelView,
) -> (Vec<String>, usize) {
    let mode = view.mode;
    let picker = view.picker;
    let rows = rows.max(1) as usize;
    let cols = cols.max(1) as usize;
    let color = state_color(snap);

    // The add picker and the per-agent detail page are DEDICATED full
    // screens — they replace the normal bar/gauges/log layout while open.
    if let Mode::AddPicker { sel } = mode {
        return render_add_screen(picker, rows, cols, sel);
    }
    // (A stale `idx` — roster shrank under us — falls through to the
    // panel; the loop's Refresh reset moves the mode off detail next tick.)
    if let Mode::AgentDetail { idx, sel } = mode
        && let Some(agent) = snap.agents.get(idx)
    {
        let actions = detail_actions(agent.role);
        return render_agent_detail(agent, &actions, sel, rows, cols);
    }

    let mut out: Vec<String> = Vec::with_capacity(rows);
    out.push(bar(snap, color, cols));

    let mut body: Vec<Vec<Span>> = Vec::new();

    // Breathing room under the bar — the bar needs negative space
    // to read as a lamp, but only once the pane can afford it.
    let breath = rows >= 4;

    // The block `ask` was here in the FIXED header, but a long question
    // could overflow a short pane and become unreadable. It now renders
    // in the SCROLLABLE content below (see `block_ask_spans` + the
    // timeline window) so it can be paged. The gutter width is still
    // needed for the `fix` line.
    let gutter = display_width(&label("ask").1);
    let ask_width = cols.saturating_sub(gutter).max(1);

    // `fix` — a broken HEAD commit tag the master must amend, shown even
    // when no plan row carries it (unknown-tag-only — codex 2bf46d9).
    // The bar already reads orange via `attention_state`.
    if let Some(c) = &snap.head_correction {
        let msg = format!(
            "fix tag: {}",
            super::status::describe_head_violation(&c.violation)
        );
        for (i, line) in wrap(&msg, ask_width).into_iter().enumerate() {
            body.push(vec![
                label(if i == 0 { "fix" } else { "" }),
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

    // Greedy fit: bar (+breath) then the gauge body until rows run out.
    // (The AGENTS + LOG sections render below, after the gauges.)
    if breath && out.len() < rows && !body.is_empty() {
        out.push(String::new());
    }
    for line in body {
        if out.len() >= rows {
            break;
        }
        out.push(emit(&line, color, cols));
    }

    // AGENTS — the focusable roster section, rendered directly so it can
    // carry the full-width focus rule and the full-row selection band.
    // The armed auto-mode reads as ▶ play / ⏸ pause (the EFFECTIVE mode
    // governing each agent's NEXT Stop-hook decision — NOT a live
    // run/stop indicator). Gated on a non-empty roster so a teamless
    // repo (and every panel-less render) is byte-for-byte unchanged.
    let agents_focused = mode.agents_focused();
    if !snap.agents.is_empty() && out.len() < rows {
        let hint = "↑↓ move · SPC play/pause · ⏎ details";
        out.push(region_rule("agents", hint, agents_focused, cols));
        // Each agent row; the cursor row gets the unified selection band.
        // The tier (master/commit/gate) distinguishes the kinds.
        for (i, a) in snap.agents.iter().enumerate() {
            if out.len() >= rows {
                break;
            }
            let spans = vec![
                plain("  ".to_string()),
                auto_mark(a.auto_mode),
                plain(format!(" {}", a.label)),
                dim(format!("  {}", tier_label(a.role))),
            ];
            out.push(row_line(&spans, mode.selected() == Some(i), color, cols));
        }
        // "+ add" button — the last selectable row (cursor index
        // `agents.len()`).
        if out.len() < rows {
            let add_selected = mode.selected() == Some(snap.agents.len());
            let spans = vec![plain("  + add agent".to_string())];
            out.push(row_line(&spans, add_selected, color, cols));
        }
        // Confirm modal — names the action, the committed-config
        // consequence, and which key is the (safe) default.
        if let Mode::Confirm { action } = mode {
            let (verb, who) = match action {
                ConfirmAction::AddCandidate { idx } => (
                    "add reviewer",
                    picker.get(idx).map(|c| c.label.as_str()).unwrap_or("?"),
                ),
                ConfirmAction::RemoveAgent { idx } => (
                    "remove reviewer",
                    snap.agents
                        .get(idx)
                        .map(|a| a.label.as_str())
                        .unwrap_or("?"),
                ),
            };
            let keys = if action.default_yes() {
                "[Y]es  [n]o  (⏎ = yes)"
            } else {
                "[y]es  [N]o  (⏎ = no)"
            };
            if out.len() < rows {
                out.push(emit(
                    &[Span(Style::Highlight, format!("confirm: {verb} “{who}”"))],
                    color,
                    cols,
                ));
            }
            if out.len() < rows {
                out.push(emit(
                    &[
                        dim("edits the committed team config · ".to_string()),
                        Span(Style::Accent, keys.to_string()),
                    ],
                    color,
                    cols,
                ));
            }
        }
    }

    // `log` — the LOWEST tier (status-tui-live-log): recent
    // activity fills whatever rows remain, MOST RECENT AT THE TOP
    // (rows arrive newest-first, git-log convention — lloyd), each
    // line styled per row kind + display-width truncated via the
    // same emit path as the gauges. A blank separator when there's
    // room for it plus at least one line.
    // Scrollable content, windowed by `offset` so it ALL pages: the block
    // ask, then the log with in-progress placeholders spliced into their
    // final slots (see `build_scroll`).
    let ask_lines = block_ask_spans(snap, cols);
    let in_prog = in_progress_rows(snap);
    let seq = build_scroll(snap, &ask_lines, &in_prog);
    let total = seq.len();
    let mut log_capacity = 0usize;
    // The log is a focusable region ONLY when there's an agents panel to
    // switch focus with — so a panel-less render keeps the bare log
    // (no rule), unchanged.
    let has_panel = !snap.agents.is_empty();
    let log_focused = mode.log_focused();
    // Render the log region when it has content OR when there's a panel
    // to switch focus with (so both focusable regions, and which one is
    // live, stay visible even with an empty log). When a panel is
    // present the LOG rule is also the breaker between panel and log.
    if out.len() < rows && (total > 0 || has_panel) {
        let mut avail = rows - out.len();
        if has_panel && avail >= 1 {
            out.push(region_rule("log", "↑↓ scroll", log_focused, cols));
            avail -= 1;
        } else if avail >= 2 {
            // Panel-less: keep the old blank separator, unchanged.
            out.push(String::new());
            avail -= 1;
        }
        log_capacity = avail;
        if total > 0 {
            // Window starting `offset` rows down, clamped so the last page
            // still fills.
            let off = offset.min(total.saturating_sub(1));
            let end = (off + avail).min(total);
            // Align summaries at one column: widest name among the windowed
            // rows — done reviews AND pending spinners (ask lines / the
            // master row have no author column).
            let author_width = seq[off..end]
                .iter()
                .filter_map(|s| match s {
                    Seg::Log(crate::cli::log::OnelineRow::Review { author, .. }) => {
                        Some(display_width(author))
                    }
                    Seg::InProg(InProgress::PendingReview { label, .. }) => {
                        Some(display_width(label))
                    }
                    _ => None,
                })
                .max()
                .unwrap_or(0);
            for s in &seq[off..end] {
                let spans = match s {
                    Seg::Ask(line) => (*line).clone(),
                    Seg::Log(row) => log_row_spans(row, author_width),
                    Seg::InProg(item) => in_progress_spans(item, frame, author_width),
                };
                out.push(emit(&spans, color, cols));
            }
        }
    }
    (out, log_capacity)
}

/// Newest-at-top render with no scroll — the common case the layout
/// tests exercise. Test-only; the live loop calls [`render_at`] directly.
#[cfg(test)]
fn render(snap: &StatusSnapshot, rows: u16, cols: u16) -> Vec<String> {
    render_at(snap, rows, cols, 0, 0, &PanelView::just(Mode::LogScroll)).0
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
                Verdict::Continue => "32",
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

/// A live, in-flight timeline row synthesized from the active plan's
/// `WaitingOn` (NOT from `log_rows`, which is historical). These sit at
/// the TOP of the timeline and carry the only animated glyph on screen.
enum InProgress {
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
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

fn spinner_glyph(frame: usize) -> &'static str {
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
fn in_progress_rows(snap: &StatusSnapshot) -> Vec<InProgress> {
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

/// Spans for one in-progress row. The pending spinner sits in the SAME
/// verdict-mark column as finished reviews (so done and in-flight align);
/// the master row mirrors a commit row (`-------` under the shas) with an
/// italic "working…" and a leading spinner for liveness.
fn in_progress_spans(item: &InProgress, frame: usize, author_width: usize) -> Vec<Span> {
    match item {
        InProgress::PendingReview { label, verb } => {
            let mark = spinner_glyph(frame);
            let after_mark = MARK_FIELD.saturating_sub(display_width(mark)) + 1;
            let author_pad = author_width.saturating_sub(display_width(label));
            vec![
                plain("  ".to_string()),
                dim(mark.to_string()),
                plain(" ".repeat(after_mark)),
                dim(format!("{label}{}", " ".repeat(author_pad))),
                // The wait-verb is what's italic — "what we await".
                Span(Style::Italic, format!(" {verb}…")),
            ]
        }
        InProgress::MasterWorking { name, verb } => vec![
            dim(format!("{} ", spinner_glyph(frame))),
            dim("------- ".to_string()),
            dim(format!("{name} ")),
            Span(Style::Italic, format!("{verb}…")),
        ],
    }
}

/// One row of scrollable content: a wrapped block-ask line, a historical
/// log row, or a live in-progress placeholder.
enum Seg<'a> {
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
fn build_scroll<'a>(
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

/// Wrapped lines for every pending (unanswered) block ask, with the
/// `ask` label gutter + accent style — the same look the fixed header
/// used, now produced as SCROLLABLE content so a long question can be
/// paged. Pure (word-wrap only): safe to call on an animation tick.
/// Empty when no ask is pending (reserves no space).
fn block_ask_spans(snap: &StatusSnapshot, cols: usize) -> Vec<Vec<Span>> {
    let gutter = display_width(&label("ask").1);
    let ask_width = cols.saturating_sub(gutter).max(1);
    let mut lines = Vec::new();
    for b in snap.blocks.iter().filter(|b| b.answer.is_none()) {
        for (i, line) in wrap(b.question.trim(), ask_width).into_iter().enumerate() {
            lines.push(vec![
                label(if i == 0 { "ask" } else { "" }),
                Span(Style::Accent, line),
            ]);
        }
    }
    lines
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
            // Mirror the wait promote scan (wait-ignores-queue-only-blocks):
            // a queued plan is promotable only if no PENDING block
            // suppresses it — a repo-wide block (plan = None) suppresses
            // ALL, a plan-scoped one suppresses its plan. Block > promote:
            // if nothing is promotable the queue is STOPPED on a human ask,
            // so show the block lamp (🙋), not the promote lamp.
            let pending = |name: &str| {
                snap.blocks.iter().any(|b| {
                    b.answer.is_none() && (b.plan.is_none() || b.plan.as_deref() == Some(name))
                })
            };
            let promotable = snap.queue.iter().find(|name| !pending(name)).cloned();
            let head = promotable.clone().unwrap_or_else(|| snap.queue[0].clone());
            let right = if snap.queue.len() > 1 {
                format!("{head} +{}", snap.queue.len() - 1)
            } else {
                head
            };
            if promotable.is_some() {
                (format!("📋 {master} promote"), right)
            } else {
                (format!("🙋 {master} blocked"), right)
            }
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
            CommitGateState::ContinuedPendingGate => "🔍",
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
    // master must self-correct before work resumes. Read the
    // `head_correction` SOURCE (set on every real violation, including
    // the unknown-tag-only case that marks no plan row — codex 2bf46d9);
    // a per-plan `MasterToFixCommitTag` row is an additional signal.
    if snap.head_correction.is_some()
        || snap
            .plans
            .iter()
            .any(|v| matches!(v.waiting_on, WaitingOn::MasterToFixCommitTag))
    {
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
        WaitingOn::MasterToContinue => "working",
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
            Style::Italic => out.push_str(&format!("\x1b[3m{piece}\x1b[0m")),
        }
        if truncated_here {
            break;
        }
    }
    out
}

/// The unified "selected" indicator: render a row as one solid
/// full-width reverse-video band, padding to `cols` so the WHOLE line
/// reads as selected. Per-span styling is dropped — the band is the
/// emphasis — and the width is display-aware (no ragged edge on a wide
/// glyph or trailing pad). One selection style, drawn one way, for
/// agent / "+ add" / picker rows alike.
fn emit_selected(spans: &[Span], cols: usize) -> String {
    let text: String = spans.iter().map(|Span(_, t)| t.as_str()).collect();
    let text = truncate_to(&text, cols);
    let pad = cols.saturating_sub(display_width(&text));
    format!("\x1b[7m{text}{}\x1b[0m", " ".repeat(pad))
}

/// Emit a panel row: the unified selection band when it's the cursor
/// row, else normal per-span styling.
fn row_line(spans: &[Span], selected: bool, color: &str, cols: usize) -> String {
    if selected {
        emit_selected(spans, cols)
    } else {
        emit(spans, color, cols)
    }
}

/// Collapse a possibly-multiline / control-laden string to ONE display
/// line: first line only, control chars dropped, truncated to `cols`.
/// Every line a renderer emits must be a single terminal row — a raw
/// `initial_prompt` with newlines would otherwise inject extra rows and
/// break the clamp.
fn one_line(s: &str, cols: usize) -> String {
    let first: String = s
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    truncate_to(&first, cols)
}

/// The full-screen "+ add" picker: a title rule, one block per
/// candidate (label + tool + the invocation that runs it, with its
/// `initial_prompt` as a dim ONE-LINE description when present), and a
/// footer. The selected candidate gets the unified selection band.
/// Output is hard-clamped to `rows` and every line is single-row, so a
/// tiny pane or a multiline prompt can never overflow the terminal.
/// Returns `(lines, 0)` — a picker has no scrollable log.
fn render_add_screen(
    picker: &[crate::cli::status::AvailableAgent],
    rows: usize,
    cols: usize,
    sel: usize,
) -> (Vec<String>, usize) {
    let mut out: Vec<String> = Vec::new();
    out.push(region_rule("add a reviewer", "", true, cols));
    out.push(String::new());
    if picker.is_empty() {
        out.push(emit(
            &[dim(
                "  no agents available — declare one with `clank agent add --global`".to_string(),
            )],
            "",
            cols,
        ));
    } else {
        for (i, c) in picker.iter().enumerate() {
            // Leave room for the blank + footer below.
            if out.len() >= rows.saturating_sub(2) {
                break;
            }
            let spans = vec![
                plain(format!("  {}", c.label)),
                dim(format!("  [{}]", c.tool)),
                plain(format!("  {}", one_line(&c.invocation, cols))),
            ];
            out.push(row_line(&spans, i == sel, "", cols));
            if let Some(desc) = &c.description
                && out.len() < rows.saturating_sub(2)
            {
                out.push(emit(
                    &[dim(format!("      {}", one_line(desc, cols)))],
                    "",
                    cols,
                ));
            }
        }
    }
    if out.len() < rows {
        out.push(String::new());
    }
    if out.len() < rows {
        out.push(emit(
            &[dim("  ↑↓ move · ⏎ add · Esc cancel".to_string())],
            "",
            cols,
        ));
    }
    // Hard backstop: never hand back more lines than the pane has.
    out.truncate(rows);
    (out, 0)
}

/// The label for a detail-page action, given the agent it acts on
/// (so "switch tier" and "toggle auto" name their destination).
fn detail_action_label(action: DetailAction, agent: &crate::cli::status::AgentAutoRow) -> String {
    use clank_core::vocab::AutoMode;
    match action {
        DetailAction::ToggleAuto => format!(
            "toggle auto (→ {})",
            if agent.auto_mode == AutoMode::On {
                "off"
            } else {
                "on"
            }
        ),
        DetailAction::SwitchTier => {
            let to = match agent.role {
                crate::cli::teams_config::RosterRole::Commit => "gate",
                _ => "commit",
            };
            format!("switch tier → {to}")
        }
        DetailAction::PromoteToMaster => "promote to master".to_string(),
        DetailAction::Remove => "remove from team".to_string(),
        DetailAction::Back => "← back".to_string(),
    }
}

/// The full-screen per-agent detail/config page: an info block (tool,
/// tier, auto, invocation, purpose) then the selectable action menu.
/// Like the picker it is hard-clamped to `rows` with single-line fields,
/// so a multiline `initial_prompt` cannot overflow. The selected action
/// gets the unified selection band. Returns `(lines, 0)` — no log.
fn render_agent_detail(
    agent: &crate::cli::status::AgentAutoRow,
    actions: &[DetailAction],
    sel: usize,
    rows: usize,
    cols: usize,
) -> (Vec<String>, usize) {
    use clank_core::vocab::AutoMode;
    let mut out: Vec<String> = Vec::new();
    out.push(region_rule(
        &format!("agent · {}", agent.label),
        "",
        true,
        cols,
    ));
    out.push(String::new());
    let auto = if agent.auto_mode == AutoMode::On {
        "▶ on"
    } else {
        "⏸ off"
    };
    let purpose = agent
        .description
        .as_deref()
        .filter(|d| !d.trim().is_empty())
        .unwrap_or("—");
    let info = [
        ("tool", agent.tool.clone()),
        ("tier", tier_label(agent.role).to_string()),
        ("auto", auto.to_string()),
        ("invocation", one_line(&agent.invocation, cols)),
        ("purpose", one_line(purpose, cols)),
    ];
    for (k, v) in info {
        if out.len() >= rows {
            break;
        }
        out.push(emit(&[dim(format!("   {k:<11}")), plain(v)], "", cols));
    }
    if out.len() < rows {
        out.push(String::new());
    }
    if out.len() < rows {
        out.push(region_rule("actions", "", false, cols));
    }
    for (i, a) in actions.iter().enumerate() {
        if out.len() >= rows.saturating_sub(1) {
            break;
        }
        let spans = vec![plain(format!("  {}", detail_action_label(*a, agent)))];
        out.push(row_line(&spans, i == sel, "", cols));
    }
    if out.len() < rows {
        out.push(String::new());
    }
    if out.len() < rows {
        out.push(emit(
            &[dim("  ↑↓ move · ⏎ select · Esc back".to_string())],
            "",
            cols,
        ));
    }
    out.truncate(rows);
    (out, 0)
}

/// A full-width titled rule marking a focusable region — the region
/// focus cue (selection is [`emit_selected`]). The FOCUSED region is a
/// solid reverse-video band carrying the title + active-key hint; the
/// unfocused one is a thin dim rule. This replaces the per-row rail and
/// the ◉/○ markers.
fn region_rule(title: &str, hint: &str, focused: bool, cols: usize) -> String {
    if focused {
        let head = if hint.is_empty() {
            format!(" {} ", title.to_uppercase())
        } else {
            format!(" {}   {hint} ", title.to_uppercase())
        };
        let head = truncate_to(&head, cols);
        let pad = cols.saturating_sub(display_width(&head));
        format!("\x1b[1;7m{head}{}\x1b[0m", " ".repeat(pad))
    } else {
        let head = truncate_to(&format!("── {} ", title.to_uppercase()), cols);
        let fill = cols.saturating_sub(display_width(&head));
        format!("\x1b[2m{head}{}\x1b[0m", "─".repeat(fill))
    }
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
struct AltScreen {
    /// The cooked-mode termios to restore on exit.
    orig: libc::termios,
}

/// The original termios as a leaked pointer so the (async-signal-
/// safe) SIGINT/SIGTERM handler can restore it without touching a
/// `static mut` (banned refs in edition 2024) or allocating.
static TERMIOS_PTR: AtomicPtr<libc::termios> = AtomicPtr::new(std::ptr::null_mut());

impl AltScreen {
    fn enter() -> Self {
        // Raw-ish input so keystrokes are READ, not
        // echoed onto the alt-screen as `^[[B`. ICANON+ECHO off; VMIN=1
        // so a stdin read blocks until ≥1 byte (the kernel notification
        // the reader thread waits on — no polling).
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        unsafe {
            libc::tcgetattr(libc::STDIN_FILENO, &mut orig);
            TERMIOS_PTR.store(Box::into_raw(Box::new(orig)), Ordering::Relaxed);
            let mut raw = orig;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
        }

        print!("{ENTER_SEQ}");
        let _ = std::io::stdout().flush();

        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &orig) };
            let mut out = std::io::stdout();
            let _ = out.write_all(RESTORE_SEQ.as_bytes());
            let _ = out.flush();
            prev(info);
        }));

        // SAFETY: installing a handler that only calls
        // async-signal-safe functions (tcsetattr, write, _exit).
        let handler =
            restore_and_exit as extern "C" fn(libc::c_int) as *const () as libc::sighandler_t;
        unsafe {
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGTERM, handler);
        }
        AltScreen { orig }
    }
}

impl Drop for AltScreen {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.orig) };
        print!("{RESTORE_SEQ}");
        let _ = std::io::stdout().flush();
    }
}

extern "C" fn restore_and_exit(sig: libc::c_int) {
    const RESTORE: &[u8] = b"\x1b[?25h\x1b[?1049l";
    // SAFETY: tcsetattr, write, _exit are async-signal-safe; the ptr is
    // a leaked Box set once in `enter`, read atomically here.
    unsafe {
        let p = TERMIOS_PTR.load(Ordering::Relaxed);
        if !p.is_null() {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, p);
        }
        libc::write(1, RESTORE.as_ptr().cast(), RESTORE.len());
        libc::_exit(128 + sig);
    }
}

/// What wakes the `--tui` loop.
enum Ev {
    /// A status/data change — probe the signature; rebuild iff it moved.
    Refresh,
    /// A terminal resize (SIGWINCH). NOT a data change: top up the
    /// (possibly taller) viewport and repaint, but never rebuild — so a
    /// resize is never swallowed by the nothing-changed gate.
    Resize,
    /// A keystroke from the stdin reader thread.
    Key(Key),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    Bottom,
    Quit,
    /// Space — page-down while scrolling the log; toggle auto on the
    /// selected agent while the agent panel is focused. Context-free
    /// parse; the loop resolves the meaning from focus.
    Space,
    /// Tab / `a` — move keyboard focus between the log and the agent
    /// panel.
    Focus,
    /// Esc — back out one level (leave the panel / cancel a picker or
    /// confirm).
    Escape,
    /// Enter — activate the row under the cursor (open the picker on
    /// "+ add", choose a candidate) or, in a confirm, follow the
    /// default.
    Enter,
    /// Backspace / DEL — remove the selected reviewer.
    Delete,
    /// `y` — confirm.
    Yes,
    /// `n` — decline.
    No,
}

/// Which region (and sub-state) owns the keyboard. The backbone of key
/// routing: each key is interpreted in exactly ONE place per mode, so
/// an unbound key does nothing and a key can't mean two things at once.
/// `Confirm` as its own mode is what makes "Enter silently confirms a
/// destructive default" unwritable — Enter is resolved in one place.
///
/// `Copy` is preserved by storing INDICES (into `snapshot.agents` /
/// the freshly-read picker list), never owned labels; the label is
/// resolved at action time. A data-changing Refresh resets the picker/
/// confirm modes (see the Refresh arm) so an index can't act on a
/// reordered target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Default: keys scroll the log.
    LogScroll,
    /// The agent panel owns the keys; `sel` is the cursor row. Rows are
    /// `agents` followed by the "+ add" row at index `agents.len()`.
    AgentPanel { sel: usize },
    /// Choosing a library agent to add; `sel` indexes the freshly-read
    /// candidate list the loop holds.
    AddPicker { sel: usize },
    /// The per-agent detail/config page; `idx` is the agent in
    /// `snapshot.agents`, `sel` the cursor over its action menu.
    AgentDetail { idx: usize, sel: usize },
    /// A mutating decision is pending; the action carries the target by
    /// index.
    Confirm { action: ConfirmAction },
}

/// One row of an agent's detail-page action menu (the actions are data
/// the cursor moves over, not a keymap). Availability depends on role —
/// see [`detail_actions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailAction {
    /// Arm/disarm the agent's auto-mode.
    ToggleAuto,
    /// Flip a reviewer between the commit and gate tiers.
    SwitchTier,
    /// Promote a reviewer to master (the core also demotes the old one).
    PromoteToMaster,
    /// Remove the agent from the team (behind the confirm).
    Remove,
    /// Leave the detail page.
    Back,
}

/// The detail-page actions for `role`, in display order. Master gets a
/// reduced set (no tier-switch / promote / remove): the UI hide is
/// primary, and the cores refuse anyway (defense in depth).
fn detail_actions(role: crate::cli::teams_config::RosterRole) -> Vec<DetailAction> {
    use crate::cli::teams_config::RosterRole;
    use DetailAction::*;
    match role {
        RosterRole::Master => vec![ToggleAuto, Back],
        RosterRole::Commit | RosterRole::Gate => {
            vec![ToggleAuto, SwitchTier, PromoteToMaster, Remove, Back]
        }
    }
}

/// What a detail-page keystroke means — PURE, like `agent_panel_action`:
/// the loop executes the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailNav {
    None,
    Quit,
    /// Esc/Tab — return to the panel.
    Back,
    /// Move the action cursor to this row.
    MoveCursor(usize),
    /// Activate the action under the cursor.
    Activate(DetailAction),
}

/// Re-locate an open detail page by LABEL after a roster rebuild:
/// the same agent's new index, or `None` if it's gone (close the page).
/// Identity is by label, never by a kept index — an external promote /
/// tier change reorders rows, so a kept index could retarget a
/// different agent.
fn relocate_detail(label: &str, agents: &[crate::cli::status::AgentAutoRow]) -> Option<usize> {
    agents.iter().position(|a| a.label == label)
}

/// Pure key routing for the detail page over its action menu.
fn agent_detail_nav(sel: usize, actions: &[DetailAction], key: Key) -> DetailNav {
    match key {
        Key::Quit => DetailNav::Quit,
        Key::Escape | Key::Focus => DetailNav::Back,
        Key::Up => DetailNav::MoveCursor(move_selection(sel, actions.len(), false)),
        Key::Down => DetailNav::MoveCursor(move_selection(sel, actions.len(), true)),
        Key::Enter | Key::Space => {
            DetailNav::Activate(actions.get(sel).copied().unwrap_or(DetailAction::Back))
        }
        _ => DetailNav::None,
    }
}

/// A pending roster mutation, by index (keeps [`Mode`] `Copy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmAction {
    /// Add the candidate at this index in the loop's picker list.
    AddCandidate { idx: usize },
    /// Remove the agent at this index in `snapshot.agents`.
    RemoveAgent { idx: usize },
}

impl ConfirmAction {
    /// Add defaults to Yes (non-destructive); remove defaults to No —
    /// so Enter (which follows the default) never confirms a removal.
    fn default_yes(self) -> bool {
        matches!(self, ConfirmAction::AddCandidate { .. })
    }
}

impl Mode {
    /// True for every agent-region sub-state (panel, picker, confirm) —
    /// the log is the only non-agents mode. Drives the focus rail/header.
    fn agents_focused(self) -> bool {
        !matches!(self, Mode::LogScroll)
    }
    fn log_focused(self) -> bool {
        matches!(self, Mode::LogScroll)
    }
    /// The agent-panel cursor row, else `None` (picker/confirm/log have
    /// no agent-row cursor).
    fn selected(self) -> Option<usize> {
        match self {
            Mode::AgentPanel { sel } => Some(sel),
            _ => None,
        }
    }
}

/// The interactive state `render_at` needs beyond the snapshot: which
/// mode owns the keyboard and the freshly-read add-picker candidates.
/// Bundled so the render signature stays small (and future interactive
/// bits land here, not as more args).
pub(crate) struct PanelView<'a> {
    pub(crate) mode: Mode,
    pub(crate) picker: &'a [crate::cli::status::AvailableAgent],
}

impl<'a> PanelView<'a> {
    /// A view with just a mode (no picker) — the common case for tests
    /// and the log-scroll default.
    #[cfg(test)]
    fn just(mode: Mode) -> Self {
        Self { mode, picker: &[] }
    }
}

/// What an `AgentPanel` keystroke means — PURE routing over the cursor
/// position and the selected row's role, so every case is unit-tested
/// and the loop only executes the result (the sole place IO happens).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelAction {
    None,
    Quit,
    /// Tab/Esc — hand focus back to the log.
    LeaveFocus,
    /// Move the cursor to this row.
    MoveCursor(usize),
    /// `Down` past the last panel row ("+ add") — cross into the log.
    EnterLog,
    /// Toggle auto on the agent at this index (SPC, quick path).
    ToggleAuto(usize),
    /// Open the per-agent detail page (Enter on an agent row).
    OpenDetail(usize),
    /// Open the add picker (Enter/Space on the "+ add" row).
    OpenPicker,
}

/// Pure key routing for the agent panel. `agents` is the roster; the
/// "+ add" row sits at index `agents.len()`. Removal/role/tier are no
/// longer panel actions — Enter opens the detail page where they live.
fn agent_panel_action(
    sel: usize,
    agents: &[crate::cli::status::AgentAutoRow],
    key: Key,
) -> PanelAction {
    let add_row = agents.len();
    let on_add = sel == add_row;
    match key {
        Key::Quit => PanelAction::Quit,
        Key::Focus | Key::Escape => PanelAction::LeaveFocus,
        Key::Up => PanelAction::MoveCursor(move_selection(sel, add_row + 1, false)),
        // `Down` past the bottom row ("+ add") flows into the log —
        // continuous navigation across the panel↔log boundary.
        Key::Down if on_add => PanelAction::EnterLog,
        Key::Down => PanelAction::MoveCursor(move_selection(sel, add_row + 1, true)),
        // Enter activates the row: the picker on "+ add", the detail page
        // on an agent. Space is the quick inline auto-toggle (or the
        // picker on "+ add").
        Key::Enter if on_add => PanelAction::OpenPicker,
        Key::Enter => PanelAction::OpenDetail(sel),
        Key::Space if on_add => PanelAction::OpenPicker,
        Key::Space => PanelAction::ToggleAuto(sel),
        _ => PanelAction::None,
    }
}

/// The log-side half of continuous navigation: pressing `Up` in the
/// log returns `Some(panel_row)` when already at the top (cross back
/// into the panel, landing on the "+ add" row adjacent to the log),
/// else `None` (just scroll up). Pure so the boundary stays tested.
fn log_up_target(offset: usize, agents_len: usize) -> Option<usize> {
    (offset == 0 && agents_len > 0).then_some(agents_len)
}

/// Resolve a Confirm keystroke: `Some(true)` confirm, `Some(false)`
/// cancel, `None` ignore. In a confirm `q` CANCELS (it does not quit
/// the TUI), and Enter follows the action's default — so a destructive
/// default is never confirmed by Enter.
fn confirm_decision(action: ConfirmAction, key: Key) -> Option<bool> {
    match key {
        Key::Yes => Some(true),
        Key::No | Key::Escape | Key::Quit => Some(false),
        Key::Enter => Some(action.default_yes()),
        _ => None,
    }
}

/// Parse a burst of stdin bytes into scroll keys — arrow keys,
/// PgUp/PgDn, plus vi-ish `j`/`k`/`g`/`G`, space (page down), `q` (quit).
fn parse_keys(bytes: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        if rest.starts_with(b"\x1b[A") {
            keys.push(Key::Up);
            i += 3;
        } else if rest.starts_with(b"\x1b[B") {
            keys.push(Key::Down);
            i += 3;
        } else if rest.starts_with(b"\x1b[5~") {
            keys.push(Key::PageUp);
            i += 4;
        } else if rest.starts_with(b"\x1b[6~") {
            keys.push(Key::PageDown);
            i += 4;
        } else if rest.starts_with(b"\x1b[") {
            // Unknown CSI (e.g. left/right arrows, F-keys): consume
            // through its final byte so a lone Esc isn't misread out of
            // the sequence's leading bytes.
            let mut j = 2;
            while j < rest.len() && !(0x40..=0x7e).contains(&rest[j]) {
                j += 1;
            }
            i += (j + 1).min(rest.len());
        } else {
            match bytes[i] {
                b'k' => keys.push(Key::Up),
                b'j' => keys.push(Key::Down),
                b'g' => keys.push(Key::Top),
                b'G' => keys.push(Key::Bottom),
                b' ' => keys.push(Key::Space),
                b'b' => keys.push(Key::PageUp),
                b'\t' | b'a' => keys.push(Key::Focus),
                b'\r' | b'\n' => keys.push(Key::Enter),
                0x7f | 0x08 => keys.push(Key::Delete),
                b'y' => keys.push(Key::Yes),
                b'n' => keys.push(Key::No),
                0x1b => keys.push(Key::Escape),
                b'q' => keys.push(Key::Quit),
                _ => {}
            }
            i += 1;
        }
    }
    keys
}

/// The opposite armed state — what a SPC toggle writes.
fn flip_auto(mode: clank_core::vocab::AutoMode) -> clank_core::vocab::AutoMode {
    use clank_core::vocab::AutoMode;
    match mode {
        AutoMode::On => AutoMode::Off,
        AutoMode::Off => AutoMode::On,
    }
}

/// The Tab/focus-key transition: from the log, enter the panel at row
/// 0 (only if there's a roster to enter); from any agent-region mode,
/// return to the log. The single focus-toggle rule, shared by every
/// handler arm.
fn toggle_focus(mode: Mode, agents_len: usize) -> Mode {
    match mode {
        Mode::LogScroll if agents_len > 0 => Mode::AgentPanel { sel: 0 },
        Mode::LogScroll => Mode::LogScroll,
        _ => Mode::LogScroll,
    }
}

/// Execute a confirmed roster mutation via the existing `clank agent`
/// cores — the TUI is a FRONT-END to add/remove, never a second write
/// path. The target label is resolved from the snapshot/picker by the
/// action's index at apply time. Adds go in as a commit reviewer
/// (parity with `clank agent add`). Best-effort: a failed core leaves
/// the roster unchanged (the panel just won't move); surfacing modal
/// errors is out of scope. The mutated `.clank/config.json` is tracked,
/// so this dirties the tree — the deliberate, committed-config change
/// the confirm modal warned about.
fn apply_confirm(
    action: ConfirmAction,
    repo: &std::path::Path,
    home: Option<&std::path::Path>,
    snap: &StatusSnapshot,
    picker: &[crate::cli::status::AvailableAgent],
) {
    match action {
        ConfirmAction::AddCandidate { idx } => {
            if let Some(c) = picker.get(idx)
                && let Ok(label) = clank_core::ids::AgentLabel::parse(&c.label)
            {
                let _ = crate::cli::agent::add_repo_roster_agent_by_name(
                    repo,
                    home,
                    &label,
                    crate::cli::teams_config::RosterRole::Commit,
                );
            }
        }
        ConfirmAction::RemoveAgent { idx } => {
            if let Some(a) = snap.agents.get(idx)
                && let Ok(label) = clank_core::ids::AgentLabel::parse(&a.label)
            {
                let _ = crate::cli::agent::remove_repo_agent(repo, &label);
            }
        }
    }
}

/// Execute a detail-page action via the existing `clank agent` cores
/// and return the next mode. ToggleAuto stays on the page (auto doesn't
/// reorder the roster); SwitchTier/Promote return to the panel (the
/// roster reorders, so leave by index and let the Refresh rebuild
/// re-bound the cursor); Remove defers to the Confirm modal. The TUI is
/// a front-end to the cores, never a reimplemented write.
fn apply_detail_action(
    action: DetailAction,
    idx: usize,
    sel: usize,
    snapshot: &mut StatusSnapshot,
    repo: &std::path::Path,
) -> Mode {
    use crate::cli::teams_config::{ReviewKind, RosterRole};
    // Copy out what we need so the &mut write below doesn't conflict.
    let (auto_mode, role, label_str) = match snapshot.agents.get(idx) {
        Some(a) => (a.auto_mode, a.role, a.label.clone()),
        None => return Mode::AgentPanel { sel: 0 },
    };
    let Ok(label) = clank_core::ids::AgentLabel::parse(&label_str) else {
        return Mode::AgentPanel { sel: idx };
    };
    match action {
        DetailAction::ToggleAuto => {
            let next = flip_auto(auto_mode);
            if crate::agent_store::set_auto_mode(repo, &label, next).is_ok() {
                snapshot.agents[idx].auto_mode = next;
            }
            Mode::AgentDetail { idx, sel }
        }
        DetailAction::SwitchTier => {
            let to = match role {
                RosterRole::Commit => ReviewKind::Gate,
                _ => ReviewKind::Commit,
            };
            let _ = crate::cli::agent::set_repo_review(repo, &label, to);
            Mode::AgentPanel { sel: idx }
        }
        DetailAction::PromoteToMaster => {
            let _ = crate::cli::agent::set_repo_master(repo, &label);
            Mode::AgentPanel { sel: 0 }
        }
        DetailAction::Remove => Mode::Confirm {
            action: ConfirmAction::RemoveAgent { idx },
        },
        DetailAction::Back => Mode::AgentPanel { sel: idx },
    }
}

/// Move the agent-panel cursor within `[0, len)`, saturating at both
/// ends (no wrap). `len == 0` pins it at 0 (an empty roster has no
/// selectable rows; the panel can't be focused then anyway).
fn move_selection(sel: usize, len: usize, down: bool) -> usize {
    let last = len.saturating_sub(1);
    if down {
        (sel + 1).min(last)
    } else {
        sel.saturating_sub(1)
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
    let _watcher = watch_status_paths(tx, &repo)?;
    // Resize is NOT a data change, so SIGWINCH gets its OWN channel and
    // maps to `Ev::Resize` — otherwise the nothing-changed gate (which
    // only fires on `Ev::Refresh`) would swallow it and leave a taller
    // pane underfilled until the next keypress.
    let (winch_tx, winch_rx) = mpsc::channel::<()>();
    spawn_sigwinch_forwarder(winch_tx)?;

    let _guard = AltScreen::enter();

    // Merge the watcher (data), SIGWINCH (resize), and stdin (keys) into
    // one event stream the loop drains. A blocking `read` on stdin IS
    // the notification (no polling).
    let (ev_tx, ev_rx) = mpsc::channel::<Ev>();
    {
        let ev_tx = ev_tx.clone();
        std::thread::spawn(move || {
            while rx.recv().is_ok() {
                if ev_tx.send(Ev::Refresh).is_err() {
                    break;
                }
            }
        });
    }
    {
        let ev_tx = ev_tx.clone();
        std::thread::spawn(move || {
            while winch_rx.recv().is_ok() {
                if ev_tx.send(Ev::Resize).is_err() {
                    break;
                }
            }
        });
    }
    std::thread::spawn(move || {
        let mut buf = [0u8; 16];
        loop {
            // SAFETY: reading our own stdin (fd 0) into a local buffer.
            let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            for key in parse_keys(&buf[..n as usize]) {
                if ev_tx.send(Ev::Key(key)).is_err() {
                    return;
                }
            }
        }
    });

    // The log grows on demand via fresh, larger
    // windowed rebuilds (the fold replays from a base, so this is
    // re-fetch-bigger, not incremental). ONE rule drives it: keep at
    // least `offset + viewport` rows loaded — that both FILLS a tall pane
    // on first paint and PAGES IN older rows as you scroll. Growth is a
    // screenful of commits per rebuild (≈ one rebuild per page, not per
    // row). `log_complete` latches once a grow returns no new rows (root).
    let mut log_window: usize = 30;
    let mut log_complete = false;

    // When inside zellij: mirror the bar's lamp emoji into the tab name,
    // and each agent's status glyph onto its own pane name.
    let mut tab = TabIndicator::new();
    let mut panes = PaneStatus::new();
    // Probe the input signature BEFORE the first build so any change
    // racing the build re-builds next wake (under-gate, never over-gate).
    let mut last_sig = crate::cli::status::input_signature(&repo).ok();
    let mut snapshot =
        StatusSnapshot::build_async(&repo, &basename, home.as_deref(), policy, None, true).await?;
    let mut offset: usize = 0;
    // Which region owns the keyboard. Starts on the log; Tab moves it to
    // the agent panel. The single source of key-routing truth.
    let mut mode = Mode::LogScroll;
    // The "+ add" candidate list — read FRESH from the global library
    // the moment the picker opens (never cached, so a `clank agent add
    // --global` elsewhere shows up at once), referenced by index while
    // AddPicker/Confirm(Add) is active, cleared when the picker closes.
    let mut picker: Vec<crate::cli::status::AvailableAgent> = Vec::new();
    // Spinner animation frame. The ONLY state an animation tick mutates.
    let mut frame: usize = 0;
    // Whether this iteration may do IO to top up the log. Set ONLY by
    // scroll (Key) and data-change (Refresh) — NEVER by an animation
    // tick. This is the structural guarantee that a tick is a pure
    // repaint: with `needs_fill == false` the fill block (the only IO in
    // the loop body besides the Refresh arm) is skipped entirely.
    let mut needs_fill = true;
    loop {
        let (rows, cols) = term_size();
        let view = PanelView {
            mode,
            picker: &picker,
        };
        let capacity = render_at(&snapshot, rows, cols, offset, frame, &view).1;

        // The ask + in-progress rows depend on blocks/waiting_on (not the
        // log fetch), so compute them before filling. `head` is the count
        // of scrollable rows that aren't log rows (ask lines + in-progress
        // placeholders); the total scrollable length is head + log.
        let ask_lines = block_ask_spans(&snapshot, cols as usize);
        let in_prog = in_progress_rows(&snapshot);
        let head = ask_lines.len() + in_prog.len();

        // Load enough log to fill the viewport AT this scroll position.
        // Gated on `needs_fill` so an animation tick never reaches it.
        if needs_fill {
            while !log_complete && head + snapshot.log_rows.len() < offset + capacity {
                log_window += capacity.max(1);
                let before = snapshot.log_rows.len();
                snapshot.log_rows = crate::cli::status::tui_log_rows(&repo, log_window).await;
                if snapshot.log_rows.len() == before {
                    log_complete = true; // hit the root — stop growing
                }
            }
            needs_fill = false;
        }

        let total = head + snapshot.log_rows.len();
        // Max offset pins the last row at the BOTTOM of the viewport.
        let max_off = total.saturating_sub(capacity.max(1));
        offset = offset.min(max_off);
        paint(&render_at(&snapshot, rows, cols, offset, frame, &view).0);

        // The in-progress rows now sit at SCATTERED indices (master after
        // the plan header; reviews in the latest commit's review block), so
        // ask `build_scroll` (the same arrangement render uses) for their
        // positions and tick iff ANY of them is within the window.
        let win = offset..offset + capacity;
        let spinner_visible = build_scroll(&snapshot, &ask_lines, &in_prog)
            .iter()
            .enumerate()
            .any(|(i, s)| matches!(s, Seg::InProg(_)) && win.contains(&i));
        let wait = if spinner_visible {
            Duration::from_millis(120)
        } else {
            Duration::from_secs(60)
        };

        match ev_rx.recv_timeout(wait) {
            // Keys only move the viewport; the loop top loads more if the
            // new position needs it.
            Ok(Ev::Key(k)) => {
                let page = (rows as usize).saturating_sub(3).max(1);
                // `mode` is Copy: matching it copies, so reassigning `mode`
                // inside an arm is free of borrow conflicts. The per-mode
                // routing is PURE (agent_panel_action / agent_detail_nav /
                // confirm_decision); the loop only executes the result
                // (where IO happens).
                match mode {
                    Mode::AgentPanel { sel } => {
                        match agent_panel_action(sel, &snapshot.agents, k) {
                            PanelAction::Quit => break,
                            PanelAction::LeaveFocus => {
                                mode = toggle_focus(mode, snapshot.agents.len())
                            }
                            PanelAction::MoveCursor(s) => mode = Mode::AgentPanel { sel: s },
                            PanelAction::EnterLog => {
                                // Cross into the log at the top.
                                offset = 0;
                                mode = Mode::LogScroll;
                            }
                            PanelAction::ToggleAuto(i) => {
                                if let Some(row) = snapshot.agents.get(i) {
                                    let next = flip_auto(row.auto_mode);
                                    if let Ok(label) =
                                        clank_core::ids::AgentLabel::parse(&row.label)
                                    {
                                        // Single source for the write
                                        // (preserves wait_timeout). On
                                        // success, flip the in-memory lamp
                                        // for an immediate repaint; the
                                        // config write also bumps the input
                                        // signature, so the watcher Refresh
                                        // reconciles to the same value.
                                        if crate::agent_store::set_auto_mode(&repo, &label, next)
                                            .is_ok()
                                        {
                                            snapshot.agents[i].auto_mode = next;
                                        }
                                    }
                                }
                            }
                            PanelAction::OpenPicker => {
                                // Read the candidates FRESH right now.
                                picker = crate::cli::status::available_agents(
                                    home.as_deref(),
                                    &snapshot.agents,
                                );
                                mode = Mode::AddPicker { sel: 0 };
                            }
                            PanelAction::OpenDetail(i) => {
                                mode = Mode::AgentDetail { idx: i, sel: 0 };
                            }
                            PanelAction::None => {}
                        }
                    }
                    // Detail page: a selectable action menu over one agent.
                    Mode::AgentDetail { idx, sel } => {
                        let role = snapshot.agents.get(idx).map(|a| a.role);
                        let actions = role.map(detail_actions).unwrap_or_default();
                        match agent_detail_nav(sel, &actions, k) {
                            DetailNav::Quit => break,
                            DetailNav::Back => mode = Mode::AgentPanel { sel: idx },
                            DetailNav::MoveCursor(s) => mode = Mode::AgentDetail { idx, sel: s },
                            DetailNav::Activate(action) => {
                                mode = apply_detail_action(action, idx, sel, &mut snapshot, &repo);
                            }
                            DetailNav::None => {}
                        }
                    }
                    // Picker: choose a candidate to add.
                    Mode::AddPicker { sel } => match k {
                        Key::Quit => break,
                        // Esc/Tab close the picker back onto the +add row.
                        Key::Escape | Key::Focus => {
                            picker.clear();
                            mode = Mode::AgentPanel {
                                sel: snapshot.agents.len(),
                            };
                        }
                        Key::Up => {
                            mode = Mode::AddPicker {
                                sel: move_selection(sel, picker.len(), false),
                            }
                        }
                        Key::Down => {
                            mode = Mode::AddPicker {
                                sel: move_selection(sel, picker.len(), true),
                            }
                        }
                        Key::Enter => {
                            if sel < picker.len() {
                                mode = Mode::Confirm {
                                    action: ConfirmAction::AddCandidate { idx: sel },
                                };
                            }
                        }
                        Key::Space
                        | Key::PageUp
                        | Key::PageDown
                        | Key::Top
                        | Key::Bottom
                        | Key::Delete
                        | Key::Yes
                        | Key::No => {}
                    },
                    // Confirm: one decision, resolved in one place.
                    Mode::Confirm { action } => {
                        if let Some(go) = confirm_decision(action, k) {
                            if go {
                                apply_confirm(action, &repo, home.as_deref(), &snapshot, &picker);
                            }
                            picker.clear();
                            // Back to the panel (on the +add row); the
                            // config write (if any) triggers a Refresh that
                            // rebuilds the roster, and its clamp re-bounds
                            // this cursor if the roster shrank.
                            mode = Mode::AgentPanel {
                                sel: snapshot.agents.len(),
                            };
                        }
                    }
                    // Default: keys scroll the log. `Up` at the very top
                    // crosses back into the panel (continuous nav).
                    Mode::LogScroll => match k {
                        Key::Quit => break,
                        Key::Focus => mode = toggle_focus(mode, snapshot.agents.len()),
                        Key::Up => match log_up_target(offset, snapshot.agents.len()) {
                            Some(sel) => mode = Mode::AgentPanel { sel },
                            None => offset = offset.saturating_sub(1),
                        },
                        Key::Down => offset += 1,
                        Key::PageUp => offset = offset.saturating_sub(page),
                        Key::Space | Key::PageDown => offset += page,
                        Key::Top => offset = 0,
                        Key::Bottom => offset = max_off,
                        Key::Escape | Key::Enter | Key::Delete | Key::Yes | Key::No => {}
                    },
                }
                needs_fill = true;
            }
            Ok(Ev::Refresh) => {
                // Nothing-changed gate (lloyd's invariant): a wake that
                // touched no snapshot input is dropped — no rebuild, no
                // log re-fold, no repaint. The probe is far cheaper than
                // the work it guards.
                let sig = crate::cli::status::input_signature(&repo).ok();
                if sig != last_sig {
                    // Capture the detail page's target by LABEL from the
                    // OLD roster before rebuilding — an external promote /
                    // tier change can REORDER rows (master, then commit,
                    // then gate), so a kept index could silently retarget a
                    // different agent. We re-locate the same label below.
                    let detail_label = match mode {
                        Mode::AgentDetail { idx, .. } => {
                            snapshot.agents.get(idx).map(|a| a.label.clone())
                        }
                        _ => None,
                    };
                    snapshot = StatusSnapshot::build_async(
                        &repo,
                        &basename,
                        home.as_deref(),
                        policy,
                        None,
                        true,
                    )
                    .await?;
                    // Restore the user's scroll depth and re-open paging in
                    // case history grew; the loop top tops up the viewport.
                    snapshot.log_rows = crate::cli::status::tui_log_rows(&repo, log_window).await;
                    last_sig = sig;
                    log_complete = false;
                    needs_fill = true;
                    // The data changed under us, so any in-flight picker/
                    // confirm (which reference now-possibly-stale indices)
                    // is cancelled back to the panel, and the panel cursor
                    // is re-bounded to the new row count (agents + the +add
                    // row). An empty roster drops focus to the log.
                    picker.clear();
                    mode = match mode {
                        Mode::LogScroll => Mode::LogScroll,
                        _ if snapshot.agents.is_empty() => Mode::LogScroll,
                        Mode::AgentPanel { sel } => Mode::AgentPanel {
                            sel: sel.min(snapshot.agents.len()),
                        },
                        // The detail page tracks ONE agent by identity:
                        // re-locate the captured label in the (possibly
                        // reordered) new roster, so an external reorder can
                        // never retarget it; if the agent is gone, close to
                        // the panel.
                        Mode::AgentDetail { sel, .. } => {
                            match detail_label
                                .as_deref()
                                .and_then(|l| relocate_detail(l, &snapshot.agents))
                            {
                                Some(idx) => Mode::AgentDetail { idx, sel },
                                None => Mode::AgentPanel {
                                    sel: snapshot.agents.len(),
                                },
                            }
                        }
                        // Cancel a picker/confirm onto the +add row.
                        Mode::AddPicker { .. } | Mode::Confirm { .. } => Mode::AgentPanel {
                            sel: snapshot.agents.len(),
                        },
                    };
                    if let Some(tab) = tab.as_mut() {
                        tab.update(&bar_emoji(&snapshot));
                    }
                    if let Some(panes) = panes.as_mut() {
                        panes.update(&snapshot);
                    }
                }
            }
            // Resize: not a data change, so no signature probe and no
            // rebuild — just top up the (possibly taller) viewport and
            // repaint. The loop top re-reads `term_size`; the fill is a
            // no-op when the viewport is already full.
            Ok(Ev::Resize) => {
                needs_fill = true;
            }
            // Animation tick: advance the frame ONLY. `needs_fill` stays
            // false, so the next iteration is a PURE repaint — render_at
            // (pure) + paint — with zero disk/git/zellij work.
            Err(mpsc::RecvTimeoutError::Timeout) if spinner_visible => {
                frame = frame.wrapping_add(1);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("event channel disconnected")
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::lifecycle::{AgentLabel, CommitSha, PlanKey};
    use clank_core::repo_state::NonEmptyVec;
    use clank_core::vocab::CommitGateState;

    #[test]
    fn parse_keys_recognizes_arrows_paging_and_vi_keys() {
        use Key::*;
        let got: Vec<_> = parse_keys(b"jk gGq")
            .iter()
            .map(|k| std::mem::discriminant(k))
            .collect();
        let want: Vec<_> = [Down, Up, Space, Top, Bottom, Quit]
            .iter()
            .map(std::mem::discriminant)
            .collect();
        assert_eq!(got, want, "vi keys + space");
        // CSI escape sequences (arrows, PgUp/PgDn), even back-to-back.
        let csi: Vec<_> = parse_keys(b"\x1b[A\x1b[B\x1b[5~\x1b[6~")
            .iter()
            .map(std::mem::discriminant)
            .collect();
        let want_csi: Vec<_> = [Up, Down, PageUp, PageDown]
            .iter()
            .map(std::mem::discriminant)
            .collect();
        assert_eq!(csi, want_csi, "arrows + page keys");
        assert!(parse_keys(b"xz.").is_empty(), "unmapped bytes are ignored");
    }

    #[test]
    fn parse_keys_recognizes_panel_focus_keys() {
        use Key::*;
        // Tab and `a` both focus the agent panel; lone Esc leaves it.
        let got: Vec<_> = parse_keys(b"\ta\x1b")
            .iter()
            .map(std::mem::discriminant)
            .collect();
        let want: Vec<_> = [Focus, Focus, Escape]
            .iter()
            .map(std::mem::discriminant)
            .collect();
        assert_eq!(got, want, "tab/a focus, esc leaves");
        // An unknown CSI (left arrow) is consumed whole — NOT misread as a
        // lone Esc that would spuriously close the panel.
        assert!(
            parse_keys(b"\x1b[D").is_empty(),
            "left arrow consumed, no stray Escape"
        );
    }

    #[test]
    fn flip_auto_inverts() {
        use clank_core::vocab::AutoMode;
        assert_eq!(flip_auto(AutoMode::On), AutoMode::Off);
        assert_eq!(flip_auto(AutoMode::Off), AutoMode::On);
    }

    #[test]
    fn toggle_focus_enters_panel_only_with_a_roster() {
        // From the log: enter the panel at row 0 — but only if there are
        // agents to focus; an empty roster stays on the log.
        assert_eq!(
            toggle_focus(Mode::LogScroll, 2),
            Mode::AgentPanel { sel: 0 }
        );
        assert_eq!(toggle_focus(Mode::LogScroll, 0), Mode::LogScroll);
        // From any agent-region mode (panel, picker, confirm): back to log.
        assert_eq!(
            toggle_focus(Mode::AgentPanel { sel: 1 }, 2),
            Mode::LogScroll
        );
        assert_eq!(toggle_focus(Mode::AddPicker { sel: 0 }, 2), Mode::LogScroll);
        assert_eq!(
            toggle_focus(
                Mode::Confirm {
                    action: ConfirmAction::RemoveAgent { idx: 0 }
                },
                2
            ),
            Mode::LogScroll
        );
    }

    #[test]
    fn move_selection_saturates_at_both_ends() {
        assert_eq!(move_selection(0, 3, false), 0, "up at top stays put");
        assert_eq!(move_selection(0, 3, true), 1, "down advances");
        assert_eq!(move_selection(2, 3, true), 2, "down at bottom stays put");
        assert_eq!(move_selection(2, 3, false), 1, "up retreats");
        assert_eq!(move_selection(0, 0, true), 0, "empty roster pins at 0");
    }

    fn agent_row(
        label: &str,
        role: crate::cli::teams_config::RosterRole,
        auto: clank_core::vocab::AutoMode,
    ) -> crate::cli::status::AgentAutoRow {
        crate::cli::status::AgentAutoRow {
            label: label.to_string(),
            role,
            auto_mode: auto,
            tool: "claude".to_string(),
            invocation: "claude".to_string(),
            description: None,
        }
    }

    #[test]
    fn auto_mark_play_pause_share_a_fixed_width_field() {
        use clank_core::vocab::AutoMode;
        // The on/off marks MUST occupy the same display width or the name
        // column jitters between rows. The fixed MARK_FIELD guarantees it.
        let on = auto_mark(AutoMode::On);
        let off = auto_mark(AutoMode::Off);
        assert_eq!(
            display_width(&on.1),
            MARK_FIELD,
            "play mark fills the field"
        );
        assert_eq!(
            display_width(&off.1),
            MARK_FIELD,
            "pause mark fills the field"
        );
        assert_eq!(display_width(&on.1), display_width(&off.1), "equal width");
    }

    #[test]
    fn agent_panel_play_pause_and_region_focus() {
        let s = two_agent_snap();

        // Log-focused: roster listed with play/pause marks; the LOG rule
        // is the highlighted (focused) one, AGENTS is the dim rule; no
        // circles, no rail, no selection band on an agent row.
        let log = render_at(&s, 40, 80, 0, 0, &PanelView::just(Mode::LogScroll)).0;
        let log_j = log.join("\n");
        assert!(
            log_j.contains('▶') && log_j.contains('⏸'),
            "play/pause marks"
        );
        assert!(
            line_with(&log, "LOG").contains(RULE_FOCUSED),
            "LOG rule highlighted when log-focused"
        );
        assert!(
            !line_with(&log, "AGENTS").contains(RULE_FOCUSED),
            "AGENTS rule dim when log-focused"
        );
        assert!(
            !log_j.contains('○') && !log_j.contains('◉') && !log_j.contains('▌'),
            "no circle markers, no rail (M3)"
        );

        // Agents-focused on codex (row 1): AGENTS rule highlights, LOG
        // dims, and the codex row is the full-row selection band.
        let ag = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView::just(Mode::AgentPanel { sel: 1 }),
        )
        .0;
        assert!(
            line_with(&ag, "AGENTS").contains(RULE_FOCUSED),
            "AGENTS rule highlighted when agents-focused"
        );
        assert!(
            !line_with(&ag, "LOG").contains(RULE_FOCUSED),
            "LOG rule dim when agents-focused"
        );
        assert!(
            line_with(&ag, "codex").contains(REVERSE),
            "selected agent row is the unified selection band"
        );
    }

    fn two_agent_snap() -> StatusSnapshot {
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;
        let mut s = snap(vec![], vec![]);
        s.agents = vec![
            agent_row("claude", RosterRole::Master, AutoMode::On),
            agent_row("codex", RosterRole::Commit, AutoMode::Off),
        ];
        s
    }

    /// The candidate list the picker renders, with invocation + desc.
    fn cand(label: &str, tool: &str, invocation: &str) -> crate::cli::status::AvailableAgent {
        crate::cli::status::AvailableAgent {
            label: label.to_string(),
            tool: tool.to_string(),
            invocation: invocation.to_string(),
            description: None,
        }
    }

    /// The single line containing `needle` (for SGR-on-the-right-line
    /// assertions), or "" if none.
    fn line_with<'a>(lines: &'a [String], needle: &str) -> &'a str {
        lines
            .iter()
            .find(|l| l.contains(needle))
            .map(String::as_str)
            .unwrap_or("")
    }

    const REVERSE: &str = "\x1b[7m"; // emit_selected band
    const RULE_FOCUSED: &str = "\x1b[1;7m"; // focused region_rule bar

    #[test]
    fn agent_panel_add_button_uses_the_unified_selection_band() {
        let s = two_agent_snap();
        // Cursor on an agent row: the "+ add" line is present but NOT the
        // selection band.
        let on_agent = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView::just(Mode::AgentPanel { sel: 0 }),
        )
        .0;
        assert!(
            line_with(&on_agent, "+ add agent").contains("+ add agent"),
            "add button present"
        );
        assert!(
            !line_with(&on_agent, "+ add agent").contains(REVERSE),
            "+ add not banded when an agent row is selected"
        );
        // Cursor on the "+ add" row (index == agents.len()): it gets the
        // SAME full-row band as a selected agent — so it's unmistakable.
        let on_add = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView::just(Mode::AgentPanel { sel: 2 }),
        )
        .0;
        assert!(
            line_with(&on_add, "+ add agent").contains(REVERSE),
            "+ add row is the unified selection band when selected"
        );
    }

    #[test]
    fn add_picker_is_full_screen_with_invocation_and_selection() {
        let s = two_agent_snap();
        let mut ruthless = cand("ruthless", "claude", "claude --model opus");
        ruthless.description = Some("tears through code".to_string());
        let picker = vec![ruthless, cand("scout", "codex", "codex")];
        let out = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView {
                mode: Mode::AddPicker { sel: 0 },
                picker: &picker,
            },
        )
        .0;
        let j = out.join("\n");
        assert!(j.contains("ADD A REVIEWER"), "full-screen title");
        // It REPLACES the normal layout: no gauges/log leak through.
        assert!(
            !j.contains("git"),
            "full screen, not the normal layout: {j}"
        );
        // Each candidate shows its invocation args + (when set) a desc.
        assert!(j.contains("claude --model opus"), "invocation args shown");
        assert!(
            j.contains("tears through code"),
            "initial_prompt description"
        );
        assert!(j.contains("scout"), "second candidate listed");
        assert!(j.contains("Esc cancel"), "footer hint");
        // The selected candidate is the unified selection band.
        assert!(
            line_with(&out, "ruthless").contains(REVERSE),
            "selected candidate is the band"
        );
    }

    #[test]
    fn add_picker_clamps_to_rows_and_collapses_multiline_descriptions() {
        let s = two_agent_snap();
        let mut multiline = cand("ruthless", "claude", "claude --model opus");
        multiline.description = Some("first line\nsecond line\nthird line".to_string());
        let picker = vec![
            multiline,
            cand("scout", "codex", "codex"),
            cand("gizmo", "claude", "claude"),
        ];
        let view = PanelView {
            mode: Mode::AddPicker { sel: 0 },
            picker: &picker,
        };

        // Tiny pane: never more lines than rows, and no element spans
        // multiple terminal rows (no embedded newline).
        let tiny = render_at(&s, 4, 40, 0, 0, &view).0;
        assert!(
            tiny.len() <= 4,
            "picker clamped to rows: got {}",
            tiny.len()
        );
        assert!(
            tiny.iter().all(|l| !l.contains('\n')),
            "every picker line is a single terminal row"
        );

        // Roomy pane: the multiline initial_prompt collapses to its
        // first line — later lines never reach the screen.
        let big = render_at(&s, 40, 60, 0, 0, &view).0.join("\n");
        assert!(big.contains("first line"), "description first line shown");
        assert!(
            !big.contains("second line") && !big.contains("third line"),
            "later description lines dropped: {big}"
        );
    }

    #[test]
    fn add_picker_empty_state_points_at_global() {
        let s = two_agent_snap();
        let out = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView::just(Mode::AddPicker { sel: 0 }),
        )
        .0
        .join("\n");
        assert!(
            out.contains("no agents available") && out.contains("--global"),
            "empty picker points at `clank agent add --global`: {out}"
        );
    }

    #[test]
    fn confirm_modal_names_action_consequence_and_default() {
        let s = two_agent_snap();
        // Remove: default No, Enter cancels.
        let rm = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView::just(Mode::Confirm {
                action: ConfirmAction::RemoveAgent { idx: 1 },
            }),
        )
        .0
        .join("\n");
        assert!(rm.contains("remove reviewer"), "names the action");
        assert!(rm.contains("codex"), "names the target");
        assert!(
            rm.contains("committed team config"),
            "names the consequence"
        );
        assert!(
            rm.contains("[N]o") && rm.contains("⏎ = no"),
            "remove default is No"
        );

        // Add: default Yes.
        let picker = vec![cand("ruthless", "claude", "claude")];
        let add = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView {
                mode: Mode::Confirm {
                    action: ConfirmAction::AddCandidate { idx: 0 },
                },
                picker: &picker,
            },
        )
        .0
        .join("\n");
        assert!(add.contains("add reviewer") && add.contains("ruthless"));
        assert!(
            add.contains("[Y]es") && add.contains("⏎ = yes"),
            "add default is Yes"
        );
    }

    #[test]
    fn panel_shows_reviewer_tiers() {
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;
        let mut s = two_agent_snap();
        s.agents = vec![
            agent_row("claude", RosterRole::Master, AutoMode::On),
            agent_row("codex", RosterRole::Commit, AutoMode::Off),
            agent_row("ruthless", RosterRole::Gate, AutoMode::On),
        ];
        let out = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView::just(Mode::AgentPanel { sel: 0 }),
        )
        .0
        .join("\n");
        // The kinds are distinguishable: master / commit / gate, not a
        // flat "reviewer".
        assert!(out.contains("master"), "master tier shown");
        assert!(out.contains("commit"), "commit tier shown");
        assert!(out.contains("gate"), "gate tier shown");
        assert!(!out.contains("reviewer"), "no flat 'reviewer' label: {out}");
    }

    #[test]
    fn detail_page_relocates_by_label_across_a_reorder() {
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;
        // Detail is open on "codex", currently at index 1.
        let before = [
            agent_row("claude", RosterRole::Master, AutoMode::On),
            agent_row("codex", RosterRole::Commit, AutoMode::Off),
            agent_row("ruthless", RosterRole::Gate, AutoMode::On),
        ];
        // After an external promote, codex moved to index 0 — relocating by
        // label finds it there, NOT whatever now sits at index 1.
        let after = [
            agent_row("codex", RosterRole::Master, AutoMode::Off),
            agent_row("claude", RosterRole::Commit, AutoMode::On),
            agent_row("ruthless", RosterRole::Gate, AutoMode::On),
        ];
        let label = before[1].label.clone();
        assert_eq!(relocate_detail(&label, &before), Some(1));
        assert_eq!(
            relocate_detail(&label, &after),
            Some(0),
            "follows the agent by label across a reorder"
        );
        // Removed entirely → None (the page should close).
        let removed = [agent_row("claude", RosterRole::Master, AutoMode::On)];
        assert_eq!(relocate_detail(&label, &removed), None);
    }

    #[test]
    fn detail_actions_are_reduced_for_master() {
        use crate::cli::teams_config::RosterRole;
        use DetailAction::*;
        assert_eq!(detail_actions(RosterRole::Master), vec![ToggleAuto, Back]);
        assert_eq!(
            detail_actions(RosterRole::Commit),
            vec![ToggleAuto, SwitchTier, PromoteToMaster, Remove, Back]
        );
        assert_eq!(
            detail_actions(RosterRole::Gate),
            vec![ToggleAuto, SwitchTier, PromoteToMaster, Remove, Back]
        );
    }

    #[test]
    fn agent_detail_nav_routes_menu_keys() {
        use DetailAction::*;
        let actions = [ToggleAuto, SwitchTier, Remove, Back];
        assert_eq!(
            agent_detail_nav(0, &actions, Key::Down),
            DetailNav::MoveCursor(1)
        );
        assert_eq!(
            agent_detail_nav(0, &actions, Key::Up),
            DetailNav::MoveCursor(0),
            "up at the top stays"
        );
        assert_eq!(
            agent_detail_nav(1, &actions, Key::Enter),
            DetailNav::Activate(SwitchTier)
        );
        assert_eq!(agent_detail_nav(0, &actions, Key::Escape), DetailNav::Back);
        assert_eq!(agent_detail_nav(0, &actions, Key::Quit), DetailNav::Quit);
    }

    #[test]
    fn detail_page_renders_info_actions_and_reduced_master_set() {
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;
        let mut s = two_agent_snap();
        let mut codex = agent_row("codex", RosterRole::Commit, AutoMode::Off);
        codex.invocation = "codex --profile deep".to_string();
        codex.description = Some("line one\nline two".to_string());
        s.agents = vec![agent_row("claude", RosterRole::Master, AutoMode::On), codex];

        // Reviewer detail (idx 1): info + full action set; the multiline
        // purpose collapses to one line; the selected action is banded.
        let rev = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView::just(Mode::AgentDetail { idx: 1, sel: 1 }),
        )
        .0;
        let rev_j = rev.join("\n");
        assert!(rev_j.contains("AGENT · CODEX"), "detail title");
        assert!(rev_j.contains("codex --profile deep"), "invocation shown");
        assert!(
            rev_j.contains("line one") && !rev_j.contains("line two"),
            "purpose one-lined"
        );
        assert!(
            rev_j.contains("switch tier → gate"),
            "tier action names the target"
        );
        assert!(rev_j.contains("promote to master") && rev_j.contains("remove from team"));
        assert!(
            line_with(&rev, "switch tier").contains(REVERSE),
            "selected action is the unified band"
        );
        // Full screen: not the normal layout.
        assert!(!rev_j.contains("git"), "detail replaces the normal layout");

        // Master detail (idx 0): reduced — no tier/promote/remove.
        let mas = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView::just(Mode::AgentDetail { idx: 0, sel: 0 }),
        )
        .0
        .join("\n");
        assert!(mas.contains("toggle auto"), "master keeps auto");
        assert!(
            !mas.contains("switch tier")
                && !mas.contains("promote to master")
                && !mas.contains("remove from team"),
            "master's action set is reduced: {mas}"
        );
    }

    #[test]
    fn agent_panel_action_routes_keys_by_row_and_role() {
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;
        let agents = vec![
            agent_row("claude", RosterRole::Master, AutoMode::On),
            agent_row("codex", RosterRole::Commit, AutoMode::Off),
        ];
        // "+ add" row is index 2 (== agents.len()): Enter AND Space open
        // the picker.
        assert_eq!(
            agent_panel_action(2, &agents, Key::Enter),
            PanelAction::OpenPicker
        );
        assert_eq!(
            agent_panel_action(2, &agents, Key::Space),
            PanelAction::OpenPicker
        );
        // Agent rows: Enter opens the detail page; Space toggles auto.
        assert_eq!(
            agent_panel_action(1, &agents, Key::Enter),
            PanelAction::OpenDetail(1)
        );
        assert_eq!(
            agent_panel_action(1, &agents, Key::Space),
            PanelAction::ToggleAuto(1)
        );
        // DEL is no longer a panel action (removal lives on the detail page).
        assert_eq!(
            agent_panel_action(1, &agents, Key::Delete),
            PanelAction::None
        );
        // Navigation: Down within the panel moves; Down at the +add row
        // (index 2 == agents.len()) crosses into the log; Tab/Esc leave;
        // q quits.
        assert_eq!(
            agent_panel_action(0, &agents, Key::Down),
            PanelAction::MoveCursor(1)
        );
        assert_eq!(
            agent_panel_action(2, &agents, Key::Down),
            PanelAction::EnterLog,
            "Down past +add flows into the log"
        );
        assert_eq!(
            agent_panel_action(0, &agents, Key::Up),
            PanelAction::MoveCursor(0),
            "Up at the top stays put"
        );
        assert_eq!(
            agent_panel_action(1, &agents, Key::Focus),
            PanelAction::LeaveFocus
        );
        assert_eq!(agent_panel_action(1, &agents, Key::Quit), PanelAction::Quit);
    }

    #[test]
    fn log_up_target_crosses_to_panel_only_at_the_top() {
        // At the top of the log, Up crosses back to the panel's +add row
        // (index == agents.len()); otherwise it scrolls (None).
        assert_eq!(log_up_target(0, 2), Some(2), "top → +add row");
        assert_eq!(log_up_target(3, 2), None, "mid-log → scroll");
        assert_eq!(log_up_target(0, 0), None, "no roster → nothing to cross to");
    }

    #[test]
    fn confirm_decision_q_cancels_and_enter_follows_default() {
        let rm = ConfirmAction::RemoveAgent { idx: 0 };
        let add = ConfirmAction::AddCandidate { idx: 0 };
        // In a confirm, q CANCELS — it must not quit the TUI.
        assert_eq!(confirm_decision(rm, Key::Quit), Some(false));
        assert_eq!(confirm_decision(rm, Key::Escape), Some(false));
        assert_eq!(confirm_decision(rm, Key::No), Some(false));
        assert_eq!(confirm_decision(rm, Key::Yes), Some(true));
        // Enter follows the default: remove cancels, add confirms.
        assert_eq!(confirm_decision(rm, Key::Enter), Some(false));
        assert_eq!(confirm_decision(add, Key::Enter), Some(true));
        assert_eq!(confirm_decision(rm, Key::Up), None);
    }

    #[test]
    fn confirm_action_default_is_safe_for_remove() {
        assert!(
            !ConfirmAction::RemoveAgent { idx: 0 }.default_yes(),
            "a destructive remove defaults to No"
        );
        assert!(
            ConfirmAction::AddCandidate { idx: 0 }.default_yes(),
            "a non-destructive add defaults to Yes"
        );
    }

    #[test]
    fn apply_confirm_remove_drops_the_reviewer_via_the_core() {
        let repo = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"claude":{"tool":"claude","role":"master"},"codex":{"tool":"codex","role":"commit"}}}"#,
        )
        .unwrap();
        let s = two_agent_snap();
        // idx 1 == codex (the reviewer).
        apply_confirm(
            ConfirmAction::RemoveAgent { idx: 1 },
            repo.path(),
            None,
            &s,
            &[],
        );
        let cfg = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        assert!(
            !cfg.contains("codex"),
            "reviewer removed from the committed roster via the core: {cfg}"
        );
        assert!(cfg.contains("claude"), "master untouched");
    }

    #[test]
    fn apply_confirm_add_inserts_from_the_library_via_the_core() {
        let repo = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
        )
        .unwrap();
        // The global library holds `ruthless` to add by name.
        std::fs::create_dir_all(home.path().join(".clank")).unwrap();
        std::fs::write(
            home.path().join(".clank/config.json"),
            r#"{"agents":{"ruthless":{"tool":"claude"}},"teams":{}}"#,
        )
        .unwrap();
        let s = two_agent_snap();
        let picker = vec![cand("ruthless", "claude", "claude")];
        apply_confirm(
            ConfirmAction::AddCandidate { idx: 0 },
            repo.path(),
            Some(home.path()),
            &s,
            &picker,
        );
        let cfg = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        assert!(
            cfg.contains("ruthless"),
            "candidate added to the committed roster via the core: {cfg}"
        );
    }

    /// A repo whose roster is claude(master) + codex(commit), matching
    /// `two_agent_snap`, for the detail-action reuse tests.
    fn detail_repo() -> tempfile::TempDir {
        let repo = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"claude":{"tool":"claude","role":"master"},"codex":{"tool":"codex","role":"commit"}}}"#,
        )
        .unwrap();
        repo
    }

    #[test]
    fn apply_detail_action_switch_tier_uses_the_core() {
        let repo = detail_repo();
        let mut s = two_agent_snap(); // idx 1 == codex (commit)
        let next = apply_detail_action(DetailAction::SwitchTier, 1, 1, &mut s, repo.path());
        let parsed: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            parsed["agents"]["codex"]["role"], "gate",
            "commit → gate via set_repo_review"
        );
        assert!(
            matches!(next, Mode::AgentPanel { .. }),
            "returns to the panel (the roster reorders)"
        );
    }

    #[test]
    fn apply_detail_action_promote_uses_the_core_and_demotes_old_master() {
        let repo = detail_repo();
        let mut s = two_agent_snap();
        apply_detail_action(DetailAction::PromoteToMaster, 1, 0, &mut s, repo.path());
        let parsed: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            parsed["agents"]["codex"]["role"], "master",
            "codex promoted"
        );
        assert_ne!(
            parsed["agents"]["claude"]["role"], "master",
            "old master demoted"
        );
    }

    #[test]
    fn apply_detail_action_toggle_auto_flips_and_stays_on_the_page() {
        use clank_core::vocab::AutoMode;
        let repo = detail_repo();
        let mut s = two_agent_snap(); // codex auto Off
        let next = apply_detail_action(DetailAction::ToggleAuto, 1, 2, &mut s, repo.path());
        assert_eq!(
            s.agents[1].auto_mode,
            AutoMode::On,
            "in-memory lamp flipped"
        );
        assert_eq!(
            next,
            Mode::AgentDetail { idx: 1, sel: 2 },
            "auto doesn't reorder, so the page persists"
        );
    }

    #[test]
    fn apply_detail_action_remove_defers_to_confirm() {
        let repo = detail_repo();
        let mut s = two_agent_snap();
        let next = apply_detail_action(DetailAction::Remove, 1, 3, &mut s, repo.path());
        assert_eq!(
            next,
            Mode::Confirm {
                action: ConfirmAction::RemoveAgent { idx: 1 }
            }
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

    #[test]
    fn pending_review_row_shows_spinner_name_and_reviewing() {
        let s = snap(vec![plan_state("foo", reviewer_missing("codex"))], vec![]);
        let out = render(&s, 40, 80).join("\n");
        assert!(out.contains(SPINNER[0]), "frame-0 spinner glyph shown");
        assert!(out.contains("codex"), "reviewer named");
        assert!(out.contains("reviewing"), "italic wait-verb");
    }

    #[test]
    fn master_row_shows_dashes_name_and_per_state_verb() {
        let s = snap(vec![plan_state("foo", WaitingOn::MasterToContinue)], vec![]);
        let out = render(&s, 40, 80).join("\n");
        assert!(out.contains("-------"), "dashes where the sha would be");
        assert!(out.contains("working"), "per-state italic verb");
        // The finalizing case (the gap M2 missed): master named + verb.
        let mut s = snap(vec![plan_state("foo", WaitingOn::MasterToFinalize)], vec![]);
        s.master = Some("claude".into());
        let out = render(&s, 40, 80).join("\n");
        assert!(out.contains("claude"), "master named");
        assert!(out.contains("finalizing"), "finalizing verb shown");
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
            agents: Vec::new(),
            shelved: Vec::new(),
            log_rows: Vec::new(),
            pr_reviews: Vec::new(),
            head_correction: None,
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
    fn unknown_tag_only_correction_is_visible_without_a_plan_row() {
        // codex 2bf46d9: a `[ghost]`-tagged code-only HEAD has NO plan
        // row to mark, but the correction must still surface — orange
        // lamp + a top-level `fix` line — not idle. Display reads the
        // `head_correction` source directly, not only the rows.
        use clank_core::wait::{HeadCorrection, HeadTagViolation};
        let mut s = snap(vec![], vec![]);
        // Baseline: no plans/queue/correction → Idle.
        assert_eq!(attention_state(&s), AttentionState::Idle);
        s.head_correction = Some(HeadCorrection {
            sha: crate::lifecycle::CommitSha::parse(&"a".repeat(40)).unwrap(),
            violation: HeadTagViolation {
                unknown: vec!["ghost".to_string()],
                untagged_touched: vec![],
                extra_named: vec![],
            },
        });
        assert_eq!(
            attention_state(&s),
            AttentionState::NeedsCorrection,
            "head_correction with no plan row still flags the lamp"
        );
        let out = render(&s, 24, 80).join("\n");
        assert!(out.contains("fix"), "top-level fix line shown: {out}");
        assert!(out.contains("ghost"), "names the bad tag: {out}");
    }

    #[test]
    fn dirty_stats_get_their_own_github_colored_line() {
        use crate::git_io::DirtyStats;
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
        use crate::git_io::DirtyStats;
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
        let s = snap(vec![], vec!["zellij-layout", "wait-hint"]);
        let v = visible(&render(&s, 1, 60)[0]);
        assert!(v.starts_with("📋 CLAUDE promote"), "got `{v}`");
        assert!(v.ends_with("zellij-layout +1"), "got `{v}`");
    }

    #[test]
    fn blocked_queued_plan_shows_block_lamp_not_promote() {
        // wait-ignores-queue-only-blocks: the only queued plan is
        // suppressed by a pending plan-scoped block — the queue is
        // STOPPED on a human ask, so the bar shows the block lamp
        // (🙋 … blocked), NOT the promote lamp.
        let mut s = snap(vec![], vec!["simctl-up"]);
        s.blocks = vec![crate::cli::block::BlockEntry {
            agent: "claude".into(),
            name: "simctl-up-design-decisions".into(),
            plan: Some("simctl-up".into()),
            question: "which sims?".into(),
            answer: None,
        }];
        let v = visible(&render(&s, 1, 60)[0]);
        assert!(v.starts_with("🙋 CLAUDE blocked"), "got `{v}`");
    }

    #[test]
    fn block_on_head_queue_item_still_shows_promote_for_lower() {
        // A block on the head queued plan must not hide a lower unblocked
        // one: block > promote applies ONLY when nothing is promotable.
        let mut s = snap(vec![], vec!["blocked-plan", "free-plan"]);
        s.blocks = vec![crate::cli::block::BlockEntry {
            agent: "claude".into(),
            name: "q".into(),
            plan: Some("blocked-plan".into()),
            question: "?".into(),
            answer: None,
        }];
        let v = visible(&render(&s, 1, 60)[0]);
        assert!(v.starts_with("📋 CLAUDE promote"), "got `{v}`");
        assert!(v.contains("free-plan"), "names the promotable item: `{v}`");
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
        // No GAUGE line repeats the actor or the plan stem (the timeline
        // below legitimately names a pending reviewer, so scope the check
        // to the gauge cluster).
        for t in &texts[1..5] {
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
        // Tall pane so the (now scrollable) ask is fully on screen.
        let texts: Vec<String> = render(&s, 12, 60).iter().map(|l| visible(l)).collect();
        assert!(texts[0].starts_with("🙋 HUMAN blocked"), "got {texts:?}");
        // The ask now renders in the SCROLLABLE region (below the compact
        // gauges), still word-wrapped under an `ask` gutter — both lines
        // present, position-independent (status-tui-block-ask-scroll).
        let joined = texts.join("\n");
        assert!(
            joined.contains("  ask  is this right?"),
            "ask line present: {texts:?}"
        );
        assert!(
            joined.contains("       more detail"),
            "wrapped continuation present: {texts:?}"
        );
    }

    #[test]
    fn long_block_ask_scrolls_into_view() {
        // A long ask overflows a short pane; the tail must be reachable by
        // scrolling (it's scrollable content, not a clipped fixed header).
        let q = "AAAA BBBB CCCC DDDD EEEE FFFF GGGG HHHH IIII JJJJ KKKK LAST";
        // FixCommitTag → no in-progress row, so this isolates ask scrolling.
        let mut s = snap(
            vec![plan_state("foo", WaitingOn::MasterToFixCommitTag)],
            vec![],
        );
        s.blocks = vec![crate::cli::block::BlockEntry {
            agent: "claude".into(),
            name: "q".into(),
            plan: None,
            question: q.into(),
            answer: None,
        }];
        // Narrow + short so the ask wraps to many lines and overflows.
        let top = render_at(&s, 8, 24, 0, 0, &PanelView::just(Mode::LogScroll))
            .0
            .join("\n");
        assert!(top.contains("AAAA"), "ask head visible at offset 0: {top}");
        assert!(!top.contains("LAST"), "ask tail off-screen at offset 0");
        // Scrolling down reveals the tail.
        let revealed = (1..30).any(|off| {
            render_at(&s, 8, 24, off, 0, &PanelView::just(Mode::LogScroll))
                .0
                .join("\n")
                .contains("LAST")
        });
        assert!(revealed, "scrolling brings the ask tail into view");
    }

    #[test]
    fn no_pending_block_reserves_no_ask_space() {
        let s = snap(
            vec![plan_state("foo", WaitingOn::MasterToFixCommitTag)],
            vec![],
        );
        assert!(
            block_ask_spans(&s, 60).is_empty(),
            "no pending block → no ask rows"
        );
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
        // MasterToFixCommitTag → no in-progress timeline row, so these
        // tests isolate LOG layout (in-progress rows covered separately).
        let mut s = snap(
            vec![plan_state("foo", WaitingOn::MasterToFixCommitTag)],
            vec![],
        );
        s.log_rows = subjects.iter().map(|l| commit_row(l)).collect();
        s
    }

    #[test]
    fn render_at_windows_the_log_and_reports_capacity() {
        let subs = [
            "row-aa", "row-bb", "row-cc", "row-dd", "row-ee", "row-ff", "row-gg", "row-hh",
        ];
        let s = snap_with_log(&subs);
        // Tall pane: capacity covers the whole log; newest shown at top.
        let (lines, cap) = render_at(&s, 40, 80, 0, 0, &PanelView::just(Mode::LogScroll));
        assert!(cap >= subs.len(), "viewport capacity reported");
        assert!(
            lines.join("\n").contains("row-aa"),
            "newest at top, offset 0"
        );
        // Scrolled down: the newest rows leave the window, older ones enter.
        let body = render_at(&s, 40, 80, 3, 0, &PanelView::just(Mode::LogScroll))
            .0
            .join("\n");
        assert!(
            !body.contains("row-aa"),
            "offset 3 scrolled past the newest"
        );
        assert!(body.contains("row-dd"), "offset 3 starts at the 4th-newest");
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
                verdict: Verdict::Continue,
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
        // The verdict tick is COLORED (green for continue) in the raw
        // ANSI output — lloyd's "make the ticks pop".
        assert!(
            raw.contains("\x1b[32m✓\x1b[0m"),
            "continue tick must be green: {raw:?}"
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
        // MasterToFixCommitTag → no in-progress row; this test isolates
        // the alignment of review marks against shas.
        let mut s = snap(
            vec![plan_state("foo", WaitingOn::MasterToFixCommitTag)],
            vec![],
        );
        s.log_rows = vec![
            commit_row("intro"),
            review(Verdict::Continue, "codex"), // ✓  (1-wide mark, mid name)
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
