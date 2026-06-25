//! The view: pure rendering of a [`StatusSnapshot`] (+ interactive
//! [`PanelView`] state) into ANSI lines. The who's-active bar, the
//! greedy-fit gauge stack, the agents panel, and the scrollable log —
//! plus the two full-screen modes (the add-picker and the per-agent
//! detail page). Builds on the text primitives, the derived state, the
//! scroll model, and the input mode; performs no IO.

use super::derive::*;
use super::input::*;
use super::scroll::*;
use super::text::*;
use crate::cli::status::{StatusSnapshot, short_sha};

/// The roster tier as shown in the UI: `master` / `commit` / `gate`.
pub(super) fn tier_label(role: crate::cli::teams_config::RosterRole) -> &'static str {
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
pub(super) fn auto_mark(mode: clank_core::vocab::AutoMode) -> Span {
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
pub(super) fn dirty_spans(d: &crate::git_io::DirtyStats) -> Vec<Span> {
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
pub(super) fn render_at(
    snap: &StatusSnapshot,
    rows: u16,
    cols: u16,
    offset: usize,
    frame: usize,
    view: &PanelView,
) -> (Vec<String>, usize) {
    let mode = view.mode;
    let picker = view.picker;
    let log_cursor = view.log_cursor;
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
            crate::cli::status::describe_head_violation(&c.violation)
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
        let url = crate::cli::status::pr_url(&pr.repo, pr.pr);
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
    if snap.plans.is_empty()
        && let Some(fp) = &snap.last_finished
    {
        body.push(vec![
            label("done"),
            dim(format!(
                "{} @ {}",
                fp.plan.as_str(),
                short_sha(fp.finalized_at.as_str())
            )),
        ]);
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
            for (local, s) in seq[off..end].iter().enumerate() {
                let spans = match s {
                    Seg::Ask(line) => (*line).clone(),
                    Seg::Log(row) => log_row_spans(row, author_width),
                    Seg::InProg(item) => in_progress_spans(item, frame, author_width),
                };
                // The selected timeline entry gets the unified selection
                // band — the same "selected" style as the panel/picker —
                // while the log is the focused region. Gated on `has_panel`
                // (the log is only a focusable region when there's a panel
                // to switch with), so a panel-less log renders bare.
                let selected = log_focused && has_panel && off + local == log_cursor;
                out.push(row_line(&spans, selected, color, cols));
            }
        }
    }
    (out, log_capacity)
}

/// Newest-at-top render with no scroll — the common case the layout
/// tests exercise. Test-only; the live loop calls [`render_at`] directly.
#[cfg(test)]
pub(super) fn render(snap: &StatusSnapshot, rows: u16, cols: u16) -> Vec<String> {
    render_at(snap, rows, cols, 0, 0, &PanelView::just(Mode::LogScroll)).0
}

/// Widest verdict mark in display columns: `✓✓` (Finished) is 2,
/// the rest are 1. Review marks sit in a field this wide so the
/// author column is constant regardless of verdict.
pub(super) const MARK_FIELD: usize = 2;

/// Style one log row for the pane. Umbrella headers get a
/// background highlight so they read as section dividers; commit
/// shas are dim at column 2 with plain subjects; review marks
/// (green ✓ / cyan ✓✓ / red ✗) start at that SAME column 2, and the
/// author is padded to `author_width` so every summary begins at one
/// aligned column (tui-log-plan-highlight-align).
pub(super) fn log_row_spans(row: &crate::cli::log::OnelineRow, author_width: usize) -> Vec<Span> {
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

/// Spans for one in-progress row. The pending spinner sits in the SAME
/// verdict-mark column as finished reviews (so done and in-flight align);
/// the master row mirrors a commit row (`-------` under the shas) with an
/// italic "working…" and a leading spinner for liveness.
pub(super) fn in_progress_spans(item: &InProgress, frame: usize, author_width: usize) -> Vec<Span> {
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

/// Wrapped lines for every pending (unanswered) block ask, with the
/// `ask` label gutter + accent style — the same look the fixed header
/// used, now produced as SCROLLABLE content so a long question can be
/// paged. Pure (word-wrap only): safe to call on an animation tick.
/// Empty when no ask is pending (reserves no space).
pub(super) fn block_ask_spans(snap: &StatusSnapshot, cols: usize) -> Vec<Vec<Span>> {
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
pub(super) fn bar(snap: &StatusSnapshot, color: &str, cols: usize) -> String {
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
pub(super) fn bar_text(snap: &StatusSnapshot) -> (String, String) {
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
pub(super) fn pr_bar_text(
    pr: &clank_core::wait::PrReviewWorkState,
    master: &str,
) -> (String, String) {
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
pub(super) fn bar_emoji(snap: &StatusSnapshot) -> String {
    bar_text(snap)
        .0
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// The full-screen "+ add" picker: a title rule, one block per
/// candidate (label + tool + the invocation that runs it, with its
/// `initial_prompt` as a dim ONE-LINE description when present), and a
/// footer. The selected candidate gets the unified selection band.
/// Output is hard-clamped to `rows` and every line is single-row, so a
/// tiny pane or a multiline prompt can never overflow the terminal.
/// Returns `(lines, 0)` — a picker has no scrollable log.
pub(super) fn render_add_screen(
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
pub(super) fn detail_action_label(
    action: DetailAction,
    agent: &crate::cli::status::AgentAutoRow,
) -> String {
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
pub(super) fn render_agent_detail(
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
