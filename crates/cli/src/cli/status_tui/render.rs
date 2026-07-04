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

/// The roster tier as shown in the UI: `master` / `commit` / `plan` /
/// `final` / `gate`.
pub(super) fn tier_label(role: crate::cli::teams_config::RosterRole) -> &'static str {
    use crate::cli::teams_config::RosterRole;
    match role {
        RosterRole::Master => "master",
        RosterRole::Commit => "commit",
        RosterRole::Plan => "plan",
        RosterRole::Final => "final",
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
        spans.push(colored("32", format!("+{}", d.insertions)));
        wrote = true;
    }
    if d.deletions > 0 {
        if wrote {
            spans.push(dim(" "));
        }
        spans.push(colored("31", format!("−{}", d.deletions)));
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

    // The block `ask` used to render here in the FIXED header; it now
    // renders full-width in the SCROLLABLE content below (see
    // `block_ask_spans`). The standard 7-col label gutter is still needed
    // for the `fix` line below, so derive it from that line's own label.
    let gutter = display_width(&label("fix").1);
    let fix_width = cols.saturating_sub(gutter).max(1);

    // `fix` — a broken HEAD commit tag the master must amend, shown even
    // when no plan row carries it (unknown-tag-only — codex 2bf46d9).
    // The bar already reads orange via `attention_state`.
    if let Some(c) = &snap.head_correction {
        let msg = format!(
            "fix tag: {}",
            crate::cli::status::describe_head_violation(&c.violation)
        );
        for (i, line) in wrap(&msg, fix_width).into_iter().enumerate() {
            body.push(vec![label(if i == 0 { "fix" } else { "" }), accent(line)]);
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

    // Queue gauge: PANEL-LESS renders only — a repo with an agents
    // panel gets the interactive QUEUE section between AGENTS and LOG
    // instead (rendering both would duplicate the list). A tall pane
    // gets the full block; otherwise one summary line.
    let queue_block = rows >= 12 && snap.queue.len() > 1;
    if !snap.queue.is_empty() && snap.agents.is_empty() {
        if queue_block {
            for (i, item) in snap.queue.iter().enumerate() {
                body.push(if i == 0 {
                    vec![label("queue"), plain(item.name.clone())]
                } else {
                    vec![label(""), dim(item.name.clone())]
                });
            }
        } else if !(snap.plans.is_empty() && snap.queue.len() == 1) {
            // (suppressed when the bar itself is the promote line
            // for the only queued item — it would be a repeat)
            let mut line = vec![label("next"), plain(snap.queue[0].name.clone())];
            if snap.queue.len() > 1 {
                line.push(dim(format!(" +{}", snap.queue.len() - 1)));
            }
            body.push(line);
        }
    }

    // `stash` gauge — PANEL-LESS renders only; a repo with an agents
    // panel gets the interactive STASH section between AGENTS and QUEUE
    // instead (rendering both would duplicate the list).
    for sv in snap.stash.iter().filter(|_| snap.agents.is_empty()) {
        let note = match (&sv.waiting_for, sv.ready) {
            (Some(w), true) => format!(" — {w} finished; pop?"),
            (Some(w), false) => format!(" — waiting on {w}"),
            (None, _) => String::new(),
        };
        body.push(vec![label("stash"), plain(sv.stem.clone()), dim(note)]);
    }

    // `pr` — active PR review(s): the full GitHub URL on its own
    // line, a clickable OSC 8 hyperlink. The bar carries the
    // actor/verb + `pr #n`; this is the addressable link.
    for pr in &snap.pr_reviews {
        let url = crate::cli::status::pr_url(&pr.repo, pr.pr);
        body.push(vec![label("pr"), link(url.clone(), url)]);
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
        // Active agents carry the spinner + italic wait-verb on their own
        // rows — activity lives where the actors are; the log below is
        // pure history (tui-spinner-on-agent-rows).
        let activity = in_progress_rows(snap);
        let verb_for = |label: &str| {
            activity.iter().find_map(|p| match p {
                InProgress::PendingReview { label: l, verb } if l == label => Some(*verb),
                InProgress::MasterWorking { name, verb } if name == label => Some(*verb),
                _ => None,
            })
        };
        for (i, a) in snap.agents.iter().enumerate() {
            if out.len() >= rows {
                break;
            }
            let mut spans = vec![
                plain("  ".to_string()),
                auto_mark(a.auto_mode),
                plain(format!(" {}", a.label)),
                dim(format!("  {}", tier_label(a.role))),
            ];
            if let Some(verb) = verb_for(&a.label) {
                spans.push(dim(format!("  {}", spinner_glyph(frame))));
                spans.push(italic(format!(" {verb}…")));
            }
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
                    &[highlight(format!("confirm: {verb} “{who}”"))],
                    color,
                    cols,
                ));
            }
            if out.len() < rows {
                out.push(emit(
                    &[
                        dim("edits the committed team config · ".to_string()),
                        accent(keys.to_string()),
                    ],
                    color,
                    cols,
                ));
            }
        }
    }

    // STASH — stashed plans ABOVE the queue (they have commits, nearer
    // to live work than queued ideas). Selection continues from the
    // panel: STASH rows at `agents.len()+1..`, QUEUE rows after them.
    // Enter reads the plan (body from the record's protective ref), `o`
    // opens its HTML page. Hidden when empty; panel-less renders keep
    // the gauge summary instead.
    if !snap.agents.is_empty() && !snap.stash.is_empty() && out.len() < rows {
        let agents_focused = mode.agents_focused();
        out.push(region_rule(
            "stash",
            "⏎ read · o page",
            agents_focused,
            cols,
        ));
        for (i, item) in snap.stash.iter().enumerate() {
            if out.len() >= rows {
                break;
            }
            let note = match (&item.waiting_for, item.ready) {
                (Some(w), true) => format!("  {w} finished — pop?"),
                (Some(w), false) => format!("  waiting on {w}"),
                (None, _) => String::new(),
            };
            let mut spans = vec![
                plain("  ".to_string()),
                plain(item.stem.clone()),
                dim(format!("  {} commit(s)", item.commits)),
            ];
            if !note.is_empty() {
                spans.push(if item.ready { accent(note) } else { dim(note) });
            }
            let sel_idx = snap.agents.len() + 1 + i;
            out.push(row_line(
                &spans,
                mode.selected() == Some(sel_idx),
                color,
                cols,
            ));
        }
    }

    // QUEUE — the queued plans between AGENTS and LOG, priority order
    // (lower NNN first). Selection continues from the panel: rows sit at
    // indices `agents.len()+1 ..` after the "+ add" row. Enter reads the
    // draft, `o` opens its HTML page, +/- nudge its priority. Hidden when
    // the queue is empty (no empty header); panel-less renders keep the
    // gauge summary above instead.
    if !snap.agents.is_empty() && !snap.queue.is_empty() && out.len() < rows {
        let agents_focused = mode.agents_focused();
        out.push(region_rule(
            "queue",
            "⏎ read · o page · +/- priority",
            agents_focused,
            cols,
        ));
        for (i, item) in snap.queue.iter().enumerate() {
            if out.len() >= rows {
                break;
            }
            let spans = vec![
                plain("  ".to_string()),
                dim(format!("{:03} ", item.priority)),
                plain(item.name.clone()),
            ];
            let sel_idx = snap.agents.len() + 1 + i;
            out.push(row_line(
                &spans,
                mode.selected() == Some(sel_idx),
                color,
                cols,
            ));
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
    let seq = build_scroll(snap, &ask_lines);
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
            // Lift on scroll (Material app-bar elevation): the flat rule
            // while the log is at its top; the same bar on a raised
            // surface while entries are scrolled UNDER it. The bar
            // settling flat is also the cue that the next Up crosses
            // into the panel (panel-focus-tops-log invariant).
            let clipped = offset.min(total.saturating_sub(1));
            out.push(if clipped > 0 {
                region_rule_elevated("log", "↑↓ scroll", log_focused, cols)
            } else {
                region_rule("log", "↑↓ scroll", log_focused, cols)
            });
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
            // Align summaries at one column: widest reviewer name among
            // the windowed rows (ask lines have no author column).
            let author_width = seq[off..end]
                .iter()
                .filter_map(|s| match s {
                    Seg::Log(crate::cli::log::OnelineRow::Review { author, .. }) => {
                        Some(display_width(author))
                    }
                    _ => None,
                })
                .max()
                .unwrap_or(0);
            for (local, s) in seq[off..end].iter().enumerate() {
                let spans = match s {
                    Seg::Ask(line) => (*line).clone(),
                    Seg::Log(row) => log_row_spans(row, author_width),
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
    use crate::cli::log::{OnelineRow, RowMarker};
    match row {
        OnelineRow::Header { plan } => {
            vec![Span(
                Style::Highlight,
                plan.clone().unwrap_or_else(|| "adhoc".to_string()),
            )]
        }
        OnelineRow::Commit {
            sha,
            subject,
            marker,
        } => {
            // The 1-col marker icon LEADS every commit row (finish `⚑`, impl
            // `⚒`, planning `✎`, adhoc `~`), then the sha, then the subject.
            // Fixed width keeps subjects column-aligned.
            let g = marker.glyph().to_string();
            let icon = match marker {
                RowMarker::Finish => colored("36", g), // cyan, like Finished
                RowMarker::AdHoc => colored("33", g),  // yellow (unchanged)
                _ => plain(g),
            };
            vec![
                icon,
                dim(format!(" {} ", &sha.as_str()[..7])),
                plain(subject.clone()),
            ]
        }
        OnelineRow::Review {
            verdict,
            author,
            summary,
        } => {
            let mark_color = verdict_color(*verdict);
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
                colored(mark_color, mark),
                plain(" ".repeat(after_mark)),
                dim(format!("{author}{}", " ".repeat(author_pad))),
                plain(snip),
            ]
        }
    }
}

/// Full-width wrapped lines for every pending (unanswered) block ask, in
/// accent style with NO label or gutter: the accent (red) already marks it
/// as a block, and a human question often needs the whole pane. Rendered as
/// SCROLLABLE content so a long question can be paged. Pure (word-wrap
/// only): safe to call on an animation tick. Empty when no ask is pending
/// (reserves no space).
pub(super) fn block_ask_spans(snap: &StatusSnapshot, cols: usize) -> Vec<Vec<Span>> {
    let width = cols.max(1);
    let mut lines = Vec::new();
    for b in snap.blocks.iter().filter(|b| b.answer.is_none()) {
        for line in wrap(b.question.trim(), width) {
            lines.push(vec![accent(line)]);
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
            let promotable = snap
                .queue
                .iter()
                .find(|item| !pending(&item.name))
                .map(|item| item.name.clone());
            let head = promotable
                .clone()
                .unwrap_or_else(|| snap.queue[0].name.clone());
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
/// A two-state segmented toggle value, e.g. `on │ off`. Positions are
/// STABLE (the active option doesn't jump sides when flipped); the active
/// one is bold, the other dim. When the row is selected, the segment is
/// flanked with dim `‹ ›` to signal ←/→/␣ cycle it.
fn toggle_segment(left: (&str, bool), right: (&str, bool), selected: bool) -> Vec<Span> {
    let opt = |(text, active): (&str, bool)| {
        if active {
            bold(text.to_string())
        } else {
            dim(text.to_string())
        }
    };
    let mut s = Vec::new();
    if selected {
        s.push(dim("‹ "));
    }
    s.push(opt(left));
    s.push(dim(" │ "));
    s.push(opt(right));
    if selected {
        s.push(dim(" ›"));
    }
    s
}

/// One detail-page row's spans: a fixed-width caret gutter (so rows don't
/// jitter as the cursor moves) then a segmented toggle (auto), a review
/// checkbox (`[x] commit` / `[ ] plan` / `[ ] final`), a glyph-prefixed
/// action, or the destructive red `✗ remove`. Selection is a `▸` caret —
/// NOT the reverse band — so the inline toggle value and the red
/// destructive cue stay visible. The caret turns red on the selected
/// Remove row as an extra danger cue.
pub(super) fn detail_row_spans(
    action: DetailAction,
    agent: &crate::cli::status::AgentAutoRow,
    selected: bool,
) -> Vec<Span> {
    use clank_core::vocab::AutoMode;
    let danger = matches!(action, DetailAction::Remove);
    // Gutter: caret+space when selected, two spaces otherwise — always 2
    // display columns, so content never shifts horizontally.
    let mut spans = vec![match (selected, danger) {
        (true, true) => colored("31", "▸ "),
        (true, false) => bold("▸ "),
        (false, _) => plain("  "),
    }];
    match action {
        DetailAction::ToggleAuto => {
            let on = agent.auto_mode == AutoMode::On;
            spans.push(dim(format!("{:<10}", "auto")));
            spans.extend(toggle_segment(("on", on), ("off", !on), selected));
        }
        DetailAction::TierCommit | DetailAction::TierPlan | DetailAction::TierFinal => {
            let kind = super::input::role_review_kind(agent.role)
                .unwrap_or(crate::cli::teams_config::ReviewKind::Commit);
            let (commit, plan, final_) = super::input::tier_boxes(kind);
            let (name, checked, label_col) = match action {
                DetailAction::TierCommit => ("commit", commit, "reviews"),
                DetailAction::TierPlan => ("plan", plan, ""),
                _ => ("final", final_, ""),
            };
            spans.push(dim(format!("{label_col:<10}")));
            // Checked → bold; unchecked → plain; plan/final render fully
            // DIM while commit is ticked (commit subsumes the gate points)
            // — still toggleable: ticking one LEAVES commit mode.
            let grayed = commit && !matches!(action, DetailAction::TierCommit);
            let box_txt = format!("[{}] {name}", if checked { "x" } else { " " });
            if selected {
                spans.push(dim("‹ "));
            }
            spans.push(match (checked, grayed) {
                (true, _) => bold(box_txt),
                (false, true) => dim(box_txt),
                (false, false) => plain(box_txt),
            });
            if selected {
                spans.push(dim(" ›"));
            }
        }
        DetailAction::PromoteToMaster => spans.push(plain("⇧ promote to master")),
        DetailAction::Remove => spans.push(colored("31", "✗ remove from team")),
        DetailAction::Back => spans.push(dim("← back")),
    }
    spans
}

/// The full-screen per-agent detail/config page: a read-only info block
/// (tool, the FULL wrapped invocation, session binding) then the
/// directly-manipulable rows — auto is an inline toggle and the review
/// tier is three checkboxes (←/→/␣ flip), promote/remove/back are
/// actions, the cursor marked by a `▸` caret. Hard-clamped to `rows`.
/// Returns `(lines, 0)` — no log.
pub(super) fn render_agent_detail(
    agent: &crate::cli::status::AgentAutoRow,
    actions: &[DetailAction],
    sel: usize,
    rows: usize,
    cols: usize,
) -> (Vec<String>, usize) {
    let mut out: Vec<String> = Vec::new();
    out.push(region_rule(
        &format!("agent · {}", agent.label),
        "",
        true,
        cols,
    ));
    out.push(String::new());
    // Read-only info — auto/review are NOT here; they're live toggle rows
    // below (one place to see AND change each, no duplication).
    if out.len() < rows {
        out.push(emit(
            &[dim(format!("   {:<11}", "tool")), plain(agent.tool.clone())],
            "",
            cols,
        ));
    }
    // The invocation in FULL, wrapped — a copy-pastable command line, never
    // `…`-truncated (a long launch command must be recoverable by eye).
    const INFO_INDENT: usize = 14; // "   " + 11-col label
    let inv_lines = wrap(&agent.invocation, cols.saturating_sub(INFO_INDENT).max(1));
    for (i, line) in inv_lines.iter().enumerate() {
        if out.len() >= rows {
            break;
        }
        let label = if i == 0 { "invocation" } else { "" };
        out.push(emit(
            &[dim(format!("   {label:<11}")), plain(line.clone())],
            "",
            cols,
        ));
    }
    // Session binding: an unbound agent can't receive work — surface that
    // as a PROBLEM, not a dash.
    if out.len() < rows {
        let mut spans = vec![dim(format!("   {:<11}", "session"))];
        match agent.session.as_deref() {
            Some(id) => spans.push(dim(one_line(id, cols.saturating_sub(INFO_INDENT)))),
            None => spans.push(colored(
                "31",
                format!("✗ unbound — run `clank as {}` in its session", agent.label),
            )),
        }
        out.push(emit(&spans, "", cols));
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
        out.push(emit(&detail_row_spans(*a, agent, i == sel), "", cols));
    }
    if out.len() < rows {
        out.push(String::new());
    }
    if out.len() < rows {
        out.push(emit(
            &[dim(
                "  ↑↓ move · ←→ ␣ change · ⏎ select · esc back".to_string()
            )],
            "",
            cols,
        ));
    }
    out.truncate(rows);
    (out, 0)
}

/// The fixed ANSI color for a verdict mark — green continue, cyan
/// finished, red request-changes, dim unmarked. Shared by the log
/// review rows and the commit-detail reviewer headers.
fn verdict_color(verdict: clank_core::vocab::Verdict) -> &'static str {
    use clank_core::vocab::Verdict;
    match verdict {
        Verdict::Continue => "32",
        Verdict::Finished => "36",
        Verdict::RequestChanges => "31",
        Verdict::Unmarked => "2",
    }
}

/// The full commit-detail content (the SINGLE layout source): the
/// commit's subject + message, then each reviewer's verdict mark + full
/// feedback body. Records the first line of each reviewer's block by
/// author so the review-focus scroll lands on the SAME lines the view
/// windows (no second function re-deriving positions that could drift).
pub(super) struct CommitLayout {
    pub(super) lines: Vec<String>,
    /// First content line (the verdict-mark row) of each reviewer's block.
    pub(super) review_line: std::collections::BTreeMap<String, usize>,
}

pub(super) fn build_commit_lines(
    short_sha: &str,
    subject: &str,
    body: &str,
    reviews: &[(String, clank_core::vocab::Verdict, String)],
    cols: usize,
) -> CommitLayout {
    let mut lines: Vec<String> = Vec::new();
    let mut review_line = std::collections::BTreeMap::new();
    // The rule title is upper-cased, so keep the (lowercase) sha out of it
    // and on the subject line with the message.
    lines.push(region_rule(
        "commit",
        "↑↓ scroll · o browser · Esc back",
        true,
        cols,
    ));
    lines.push(String::new());
    // WRAP the subject (don't truncate) so a long summary is fully readable:
    // the sha leads the first line; continuations align under the subject.
    let sha_prefix = format!("{short_sha}  ");
    let gutter = display_width(&sha_prefix);
    let subject_line: String = subject
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let wrapped = wrap(&subject_line, cols.saturating_sub(gutter).max(1));
    if wrapped.is_empty() {
        lines.push(emit(&[dim(sha_prefix.clone())], "", cols));
    } else {
        for (i, seg) in wrapped.into_iter().enumerate() {
            if i == 0 {
                lines.push(emit(&[dim(sha_prefix.clone()), plain(seg)], "", cols));
            } else {
                lines.push(emit(
                    &[plain(format!("{}{}", " ".repeat(gutter), seg))],
                    "",
                    cols,
                ));
            }
        }
    }
    lines.push(String::new());
    for line in wrap(body, cols) {
        lines.push(emit(&[plain(line)], "", cols));
    }
    for (author, verdict, rbody) in reviews {
        lines.push(String::new());
        // Anchor on the verdict-mark row (after the separating blank), so
        // focusing a reviewer puts its header at the top of the view.
        review_line.insert(author.clone(), lines.len());
        let mark = crate::cli::log::verdict_mark(*verdict, false);
        lines.push(emit(
            &[
                colored(verdict_color(*verdict), mark),
                plain(format!(" {author}")),
            ],
            "",
            cols,
        ));
        for line in wrap(rbody, cols) {
            lines.push(emit(&[dim(line)], "", cols));
        }
    }
    CommitLayout { lines, review_line }
}

/// The full-window commit-detail view (the `Enter`-on-a-log-entry drill
/// in), scrolled by `offset`. Read-only, like the add-picker /
/// agent-detail screens. Returns the windowed lines (clamped to `rows`)
/// AND the TOTAL content height, so the loop clamps the scroll offset to
/// the last page.
pub(super) fn render_commit_detail(
    short_sha: &str,
    subject: &str,
    body: &str,
    reviews: &[(String, clank_core::vocab::Verdict, String)],
    offset: usize,
    rows: usize,
    cols: usize,
) -> (Vec<String>, usize) {
    let layout = build_commit_lines(short_sha, subject, body, reviews, cols);
    let total = layout.lines.len();
    let off = offset.min(total.saturating_sub(1));
    let windowed = layout.lines.into_iter().skip(off).take(rows).collect();
    (windowed, total)
}

/// The scroll offset that brings reviewer `author`'s feedback to the top
/// of the commit-detail view (its block's first line), falling back to
/// the topmost reviewer block, then 0. Derived from the SAME layout the
/// view windows, so the focus can't drift from what's drawn.
pub(super) fn commit_review_offset(
    short_sha: &str,
    subject: &str,
    body: &str,
    reviews: &[(String, clank_core::vocab::Verdict, String)],
    author: &str,
    cols: usize,
) -> usize {
    let layout = build_commit_lines(short_sha, subject, body, reviews, cols);
    layout
        .review_line
        .get(author)
        .copied()
        .or_else(|| layout.review_line.values().copied().min())
        .unwrap_or(0)
}

/// The full-window plan-document view (the `Enter`-on-a-plan-header drill
/// in): the plan stem as the rule title, then the plan's markdown rendered
/// to styled lines (or a dim "plan file not found" when the file is
/// absent), scrolled by `offset`. Returns the windowed lines (clamped to
/// `rows`) AND the TOTAL content height, so the loop clamps the offset to
/// the last page — same shape as [`render_commit_detail`].
pub(super) fn render_plan_doc(
    stem: &str,
    markdown: Option<&str>,
    offset: usize,
    rows: usize,
    cols: usize,
) -> (Vec<String>, usize) {
    let mut content: Vec<String> = Vec::new();
    content.push(region_rule(
        stem,
        "↑↓ scroll · o browser · Esc back",
        true,
        cols,
    ));
    content.push(String::new());
    match markdown {
        Some(md) => content.extend(super::markdown::render_markdown(md, cols)),
        None => content.push(emit(&[dim("plan file not found")], "", cols)),
    }
    let total = content.len();
    let off = offset.min(total.saturating_sub(1));
    let windowed = content.into_iter().skip(off).take(rows).collect();
    (windowed, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::status_tui::fixtures::{
        REVERSE, agent_row, cand, line_with, plan_state, pr_work, reviewer_missing, snap,
        two_agent_snap, visible, visible_untrimmed,
    };
    use crate::lifecycle::{AgentLabel, CommitSha, PlanKey};
    use clank_core::plan_view::WaitingOn;

    fn commit_row(subject: &str) -> crate::cli::log::OnelineRow {
        crate::cli::log::OnelineRow::Commit {
            sha: crate::lifecycle::CommitSha::parse(&format!("{:0<40}", "abc1234")).unwrap(),
            subject: subject.to_string(),
            marker: crate::cli::log::RowMarker::Plain,
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
    fn row_marker_glyphs_are_one_display_column() {
        // Alignment invariant (ruthless ab4e174): every commit-row marker
        // glyph must be ONE display column per the pane's OWN width model
        // (`char_width`), or the fixed 1-col gutter breaks. A future emoji
        // swap (🔨/📜/🏁 → width 2) fails here.
        use crate::cli::log::RowMarker;
        for m in [
            RowMarker::Plain,
            RowMarker::AdHoc,
            RowMarker::Planning,
            RowMarker::Impl,
            RowMarker::Finish,
        ] {
            assert_eq!(
                super::char_width(m.glyph()),
                1,
                "{m:?} glyph must be 1 display column"
            );
        }
    }

    #[test]
    fn log_rule_lifts_while_entries_are_scrolled_under_it() {
        // Material lift-on-scroll: the LOG rule is flat (dim, no bg) at
        // the top; scrolled down, the same bar renders on the raised
        // surface. The bar settling flat is also the visual cue that the
        // next Up crosses into the panel (panel-focus-tops-log).
        let mut s = two_agent_snap();
        s.log_rows = (0..12).map(|i| commit_row(&format!("c{i}"))).collect();
        let view = PanelView {
            mode: Mode::LogScroll,
            picker: &[],
            log_cursor: 6,
        };
        let rule_of = |offset: usize| {
            let out = render_at(&s, 12, 60, offset, 0, &view).0;
            out.iter()
                .find(|l| visible(l).contains("LOG"))
                .expect("log rule")
                .clone()
        };
        assert!(
            !rule_of(0).contains("48;5;238"),
            "topped → flat rule, no raised surface"
        );
        assert!(
            rule_of(6).contains("48;5;238"),
            "scrolled → the bar lifts onto the raised surface"
        );
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
            // plan commit: sha at col 2, empty (space) marker gutter,
            // then the subject (adhoc-commit-marker).
            texts
                .iter()
                .any(|t| t.starts_with("  abc1234") && t.contains("intro")),
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
        // The `fix` line keeps its standard 7-col label gutter ("  fix  ")
        // — pins the do-not-touch boundary after the block ask dropped its
        // own gutter (status-tui-drop-block-ask-label).
        assert!(
            out.contains("  fix  "),
            "fix line keeps its 7-col gutter: {out}"
        );
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
        // The ask renders FULL-WIDTH in accent with no `ask` label/gutter
        // (status-tui-drop-block-ask-label): both lines start at column 0.
        let joined = texts.join("\n");
        assert!(
            texts.iter().any(|t| t.starts_with("is this right?")),
            "ask first line full-width, no label: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.starts_with("more detail")),
            "wrapped continuation full-width, no gutter: {texts:?}"
        );
        assert!(
            !joined.contains("ask  is this right?"),
            "the `ask` label gutter is gone: {texts:?}"
        );
    }

    #[test]
    fn long_block_ask_scrolls_into_view() {
        // A long ask overflows a short pane; the tail must be reachable by
        // scrolling (it's scrollable content, not a clipped fixed header).
        // Long enough to overflow the short pane even at FULL width — the
        // ask no longer wraps under a 7-col gutter (drop-block-ask-label),
        // so it needs more words to still spill off-screen.
        let q = format!("AAAA {}LAST", "MID ".repeat(40));
        let q = q.as_str();
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
    fn focus_is_shown_by_the_item_band_not_a_section_highlight() {
        let s = two_agent_snap();

        // The region rules are NEVER highlighted (no reverse-video bar);
        // focus is shown by the selected ITEM band, which sits in the
        // focused region. No circles, no rail either.
        let log = render_at(&s, 40, 80, 0, 0, &PanelView::just(Mode::LogScroll)).0;
        let log_j = log.join("\n");
        assert!(
            log_j.contains('▶') && log_j.contains('⏸'),
            "play/pause marks"
        );
        assert!(
            !log_j.contains("\x1b[1;7m"),
            "no section highlight (reverse-video rule): {log_j}"
        );
        assert!(
            !log_j.contains('○') && !log_j.contains('◉') && !log_j.contains('▌'),
            "no circle markers, no rail"
        );

        // Agents-focused on codex (row 1): the focus cue is the codex
        // row's selection band — and STILL no section highlight.
        let ag = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView::just(Mode::AgentPanel { sel: 1 }),
        )
        .0;
        let ag_j = ag.join("\n");
        assert!(
            !ag_j.contains("\x1b[1;7m"),
            "no section highlight when agents-focused either"
        );
        assert!(
            line_with(&ag, "codex").contains(REVERSE),
            "the selected item band is the focus cue"
        );
    }

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
                log_cursor: 0,
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
            log_cursor: 0,
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
                log_cursor: 0,
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
    fn detail_page_renders_info_actions_and_reduced_master_set() {
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;
        let mut s = two_agent_snap();
        let mut codex = agent_row("codex", RosterRole::Commit, AutoMode::Off);
        codex.invocation = "codex --profile deep".to_string();
        codex.session = Some("0d9af2c1-session-id".to_string());
        s.agents = vec![agent_row("claude", RosterRole::Master, AutoMode::On), codex];

        // Reviewer detail (idx 1): info + full action set; the review tier
        // renders as checkboxes; the selected row carries the caret.
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
        assert!(rev_j.contains("0d9af2c1-session-id"), "bound session shown");
        assert!(!rev_j.contains("purpose"), "the dead purpose row is gone");
        // The review tier is three checkboxes exposing the REAL domain —
        // a Commit reviewer: [x] commit, with plan/final unticked.
        assert!(rev_j.contains("[x] commit"), "commit checked: {rev_j}");
        assert!(rev_j.contains("[ ] plan"), "plan unticked");
        assert!(rev_j.contains("[ ] final"), "final unticked");
        assert!(!rev_j.contains("gate"), "no internal 'gate' word: {rev_j}");
        assert!(rev_j.contains("promote to master") && rev_j.contains("remove from team"));
        // Selected row (the commit checkbox, sel=1) is marked by the ▸
        // caret — not the reverse-video band.
        assert!(
            line_with(&rev, "[x] commit").contains('▸'),
            "selected row carries the caret"
        );
        // Full screen: not the normal layout.
        assert!(!rev_j.contains("git"), "detail replaces the normal layout");

        // Master detail (idx 0): reduced — auto toggle + back only, and its
        // session (unset in the fixture) reads as a red problem.
        let mas = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView::just(Mode::AgentDetail { idx: 0, sel: 0 }),
        )
        .0;
        let mas_j = mas.join("\n");
        assert!(
            mas_j.contains("auto") && mas_j.contains("on") && mas_j.contains("off"),
            "master keeps the auto toggle: {mas_j}"
        );
        assert!(
            !mas_j.contains("[ ] plan") && !mas_j.contains("promote") && !mas_j.contains("remove"),
            "master's action set is reduced: {mas_j}"
        );
        assert!(
            mas_j.contains("unbound") && mas_j.contains("clank as claude"),
            "unbound session surfaced as a problem with the fix: {mas_j}"
        );
        assert!(
            line_with(&mas, "unbound").contains("\x1b[31m"),
            "unbound renders red"
        );
    }

    #[test]
    fn queue_section_renders_between_agents_and_log_with_selection() {
        // Two agents + two queued items: the QUEUE section sits between
        // AGENTS and LOG, priority-ordered, and the panel selection
        // continues past "+ add" into the queue rows.
        let mut s = two_agent_snap();
        s.queue = vec![
            crate::cli::status::QueueItemView {
                priority: 100,
                name: "urgent-fix".into(),
            },
            crate::cli::status::QueueItemView {
                priority: 800,
                name: "later-idea".into(),
            },
        ];
        // sel 3 = first queue row (0-1 agents, 2 "+ add").
        let out = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView::just(Mode::AgentPanel { sel: 3 }),
        )
        .0;
        let texts: Vec<String> = out.iter().map(|l| visible(l)).collect();
        let pos = |needle: &str| texts.iter().position(|t| t.contains(needle));
        let (agents_rule, queue_rule, log_rule) = (
            pos("AGENTS").expect("agents rule"),
            pos("QUEUE").expect("queue rule"),
            pos("LOG").expect("log rule"),
        );
        assert!(
            agents_rule < queue_rule && queue_rule < log_rule,
            "QUEUE sits between AGENTS and LOG"
        );
        assert!(
            texts.iter().any(|t| t.contains("100 urgent-fix"))
                && texts.iter().any(|t| t.contains("800 later-idea")),
            "queue rows show priority + name: {texts:?}"
        );
        let a = pos("urgent-fix").unwrap();
        let b = pos("later-idea").unwrap();
        assert!(a < b, "priority order (lower NNN first)");
        // The selected queue row carries the unified selection band.
        let row_idx = texts
            .iter()
            .position(|t| t.contains("100 urgent-fix"))
            .expect("queue row");
        assert!(
            out[row_idx].contains(REVERSE),
            "queue row selection band: {:?}",
            out[row_idx]
        );
        assert!(
            texts.iter().any(|t| t.contains("+/- priority")),
            "reprioritise hint shown"
        );

        // Empty queue: the section vanishes entirely.
        let s = two_agent_snap();
        let out = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView::just(Mode::AgentPanel { sel: 0 }),
        )
        .0;
        assert!(
            !out.iter().any(|l| visible(l).contains("QUEUE")),
            "no empty QUEUE header"
        );
    }

    #[test]
    fn stash_section_renders_above_the_queue_with_ready_nudge() {
        let mut s = two_agent_snap();
        s.stash = vec![crate::cli::status::StashItemView {
            stem: "parked".into(),
            waiting_for: Some("dep".into()),
            ready: true,
            commits: 3,
        }];
        s.queue = vec![crate::cli::status::QueueItemView {
            priority: 500,
            name: "queued-idea".into(),
        }];
        let out = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView::just(Mode::AgentPanel { sel: 3 }),
        )
        .0;
        let texts: Vec<String> = out.iter().map(|l| visible(l)).collect();
        let pos = |needle: &str| texts.iter().position(|t| t.contains(needle));
        let (agents_rule, stash_rule, queue_rule, log_rule) = (
            pos("AGENTS").expect("agents rule"),
            pos("STASH").expect("stash rule"),
            pos("QUEUE").expect("queue rule"),
            pos("LOG").expect("log rule"),
        );
        assert!(
            agents_rule < stash_rule && stash_rule < queue_rule && queue_rule < log_rule,
            "order: AGENTS → STASH → QUEUE → LOG"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.contains("parked") && t.contains("3 commit(s)")),
            "stash row shows name + commit count: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("dep finished — pop?")),
            "ready nudge shown"
        );
        // sel 3 = the stash row (first row after "+ add") carries the band.
        let row = texts.iter().position(|t| t.contains("parked")).unwrap();
        assert!(out[row].contains(REVERSE), "stash row selection band");

        // Empty stash: the section vanishes entirely.
        let s = two_agent_snap();
        let out = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView::just(Mode::AgentPanel { sel: 0 }),
        )
        .0;
        assert!(
            !out.iter().any(|l| visible(l).contains("STASH")),
            "no empty STASH header"
        );
    }

    #[test]
    fn detail_page_gate_reviewer_shows_plan_and_final_ticked() {
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;
        let mut s = two_agent_snap();
        s.agents = vec![
            agent_row("claude", RosterRole::Master, AutoMode::On),
            agent_row("ruthless", RosterRole::Gate, AutoMode::On),
        ];
        let out = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView::just(Mode::AgentDetail { idx: 1, sel: 0 }),
        )
        .0
        .join("\n");
        // Gate == plan+final: both ticked, commit unticked.
        assert!(out.contains("[ ] commit"), "commit unticked: {out}");
        assert!(out.contains("[x] plan"), "plan ticked");
        assert!(out.contains("[x] final"), "final ticked");
    }

    #[test]
    fn detail_page_invocation_wraps_in_full_without_ellipsis() {
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;
        let mut s = two_agent_snap();
        let mut codex = agent_row("codex", RosterRole::Commit, AutoMode::Off);
        codex.invocation =
            "codex --profile deep --sandbox danger-full-access --config model_reasoning_effort=high"
                .to_string();
        // A short bound session so no OTHER info row needs truncation at
        // this narrow width — the no-ellipsis assert below is global.
        codex.session = Some("s1".to_string());
        s.agents = vec![agent_row("claude", RosterRole::Master, AutoMode::On), codex];
        let out = render_at(
            &s,
            40,
            48, // narrow: the invocation cannot fit one line
            0,
            0,
            &PanelView::just(Mode::AgentDetail { idx: 1, sel: 0 }),
        )
        .0;
        // The FULL command is present (copy-pastable) — wrapped, never
        // `…`-truncated. Reassemble the visible text and check every token.
        let all: String = out.iter().map(|l| visible(l)).collect::<Vec<_>>().join(" ");
        for token in [
            "codex",
            "--profile",
            "deep",
            "--sandbox",
            "danger-full-access",
            "model_reasoning_effort=high",
        ] {
            assert!(all.contains(token), "token `{token}` lost: {all}");
        }
        assert!(
            !out.iter().any(|l| visible(l).contains('…')),
            "invocation must never be ellipsized"
        );
    }

    #[test]
    fn log_focused_highlights_the_cursor_entry() {
        let mut s = two_agent_snap(); // has_panel
        s.log_rows = vec![
            commit_row("first"),
            commit_row("second"),
            commit_row("third"),
        ];
        // Log focused, cursor on the SECOND entry (seq index 1, since
        // two_agent_snap has no ask/in-progress rows).
        let view = PanelView {
            mode: Mode::LogScroll,
            picker: &[],
            log_cursor: 1,
        };
        let lines = render_at(&s, 40, 80, 0, 0, &view).0;
        assert!(
            line_with(&lines, "second").contains(REVERSE),
            "the cursor entry carries the unified selection band"
        );
        assert!(
            !line_with(&lines, "first").contains(REVERSE),
            "non-cursor entries are not banded"
        );
        // Focus the panel instead: no log entry is banded.
        let panel = render_at(
            &s,
            40,
            80,
            0,
            0,
            &PanelView::just(Mode::AgentPanel { sel: 0 }),
        )
        .0;
        assert!(
            !line_with(&panel, "second").contains(REVERSE),
            "no log cursor band when the panel is focused"
        );
    }

    #[test]
    fn active_reviewer_agent_row_carries_spinner_and_verb() {
        // Activity lives on the AGENTS rows (tui-spinner-on-agent-rows):
        // the pending reviewer's row spins with the italic wait-verb, the
        // idle agent's row does not, and the LOG carries no placeholder
        // rows — "waiting is never invisible" now holds via the panel.
        let mut s = two_agent_snap(); // claude (master) + codex (commit)
        s.plans = vec![plan_state("foo", reviewer_missing("codex"))];
        let out = render(&s, 40, 80);
        let codex_row = visible(line_with(&out, "codex"));
        assert!(
            codex_row.contains(SPINNER[0]),
            "spinner on the actor's row: {codex_row}"
        );
        assert!(codex_row.contains("reviewing"), "wait-verb: {codex_row}");
        let claude_row = visible(line_with(&out, "claude"));
        assert!(
            !claude_row.contains(SPINNER[0]) && !claude_row.contains("reviewing"),
            "idle agent row unchanged: {claude_row}"
        );

        // The documented teamless trade: no panel → no spinner anywhere
        // (activity presupposes a roster).
        let teamless = snap(vec![plan_state("foo", reviewer_missing("codex"))], vec![]);
        assert!(
            !render(&teamless, 40, 80).join("\n").contains(SPINNER[0]),
            "panel-less render carries no spinner"
        );
    }

    #[test]
    fn master_agent_row_spins_with_the_producing_verb() {
        // Any master-producing state spins MASTER's row with the per-state
        // verb — incl. finalizing (the gap M2 once missed).
        let mut s = two_agent_snap();
        s.plans = vec![plan_state("foo", WaitingOn::MasterToContinue)];
        let row = visible(line_with(&render(&s, 40, 80), "claude"));
        assert!(
            row.contains(SPINNER[0]) && row.contains("working"),
            "producing master spins: {row}"
        );
        let mut s = two_agent_snap();
        s.plans = vec![plan_state("foo", WaitingOn::MasterToFinalize)];
        let row = visible(line_with(&render(&s, 40, 80), "claude"));
        assert!(
            row.contains(SPINNER[0]) && row.contains("finalizing"),
            "finalizing verb shown: {row}"
        );
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
        // The long first line wrapped onto multiple full-width ask rows
        // (no `ask` gutter now — status-tui-drop-block-ask-label).
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
    fn commit_detail_shows_message_and_each_reviewers_feedback() {
        use clank_core::vocab::Verdict;
        let reviews = vec![
            (
                "codex".to_string(),
                Verdict::Continue,
                "looks good to me".to_string(),
            ),
            (
                "ruthless".to_string(),
                Verdict::RequestChanges,
                "fix the thing".to_string(),
            ),
        ];
        let (lines, total) = render_commit_detail(
            "abc1234",
            "do the thing",
            "a longer body paragraph",
            &reviews,
            0,
            40,
            60,
        );
        let joined = lines
            .iter()
            .map(|l| visible(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("abc1234"), "heading shows the short sha");
        assert!(joined.contains("do the thing"), "subject shown");
        assert!(
            joined.contains("a longer body paragraph"),
            "full body shown"
        );
        assert!(
            joined.contains("codex") && joined.contains("looks good to me"),
            "reviewer 1 verdict author + body"
        );
        assert!(
            joined.contains("ruthless") && joined.contains("fix the thing"),
            "reviewer 2 verdict author + body"
        );
        assert!(total >= lines.len(), "reports total content height");
        assert!(lines.len() <= 40, "full-window: clamped to the row budget");

        // Scrolling shifts the window — the heading is no longer line 0.
        let (scrolled, _) = render_commit_detail(
            "abc1234",
            "do the thing",
            "a longer body paragraph",
            &reviews,
            3,
            40,
            60,
        );
        assert_ne!(
            visible(&scrolled[0]),
            visible(&lines[0]),
            "offset shifts the visible window"
        );
    }

    #[test]
    fn commit_detail_wraps_a_long_subject_instead_of_truncating() {
        // A subject wider than the pane must WRAP across multiple lines with
        // every word readable — never truncated with an ellipsis.
        let subject =
            "[a-really-long-plan-name] make the thing work end to end without going off the screen";
        let cols = 40;
        let (lines, _) = render_commit_detail("abc1234", subject, "", &[], 0, 40, cols);
        let visibles: Vec<String> = lines.iter().map(|l| visible(l)).collect();
        let joined = visibles.join("\n");
        // Every word of the subject survives (nothing dropped to an
        // ellipsis) — the strongest anti-truncation check.
        for word in subject.split_whitespace() {
            assert!(joined.contains(word), "word `{word}` missing: {joined}");
        }
        // The sha still leads the first subject line, and the subject spilled
        // onto more than one line (i.e. it actually wrapped).
        let sha_line = visibles
            .iter()
            .position(|l| l.contains("abc1234"))
            .expect("sha line present");
        assert!(visibles[sha_line].contains("[a-really-long-plan-name]"));
        let last_word_line = visibles
            .iter()
            .position(|l| l.contains("screen"))
            .expect("last word present");
        assert!(
            last_word_line > sha_line,
            "subject wrapped onto a later line: {joined}"
        );
        // Nothing exceeds the pane width.
        for l in &visibles {
            assert!(
                display_width(l.trim_end()) <= cols,
                "line wider than {cols}: {l:?}"
            );
        }
    }

    #[test]
    fn commit_review_offset_lands_on_the_reviewer_block() {
        use clank_core::vocab::Verdict;
        let reviews = vec![
            (
                "aaa".to_string(),
                Verdict::Continue,
                "first body".to_string(),
            ),
            (
                "zzz".to_string(),
                Verdict::RequestChanges,
                "second body".to_string(),
            ),
        ];
        let cols = 60;
        let layout = build_commit_lines("abc1234", "subject", "the message body", &reviews, cols);
        // Each recorded line is that reviewer's header (verdict-mark) row —
        // the SAME lines the view windows, so the focus can't drift.
        for (author, _, _) in &reviews {
            let at = layout.review_line[author];
            assert!(
                visible(&layout.lines[at]).contains(author),
                "review_line[{author}] is the author's header row"
            );
        }
        // A known author → its block; an unknown author → the topmost
        // block; no reviews → 0.
        let z = commit_review_offset(
            "abc1234",
            "subject",
            "the message body",
            &reviews,
            "zzz",
            cols,
        );
        assert_eq!(z, layout.review_line["zzz"]);
        let ghost = commit_review_offset(
            "abc1234",
            "subject",
            "the message body",
            &reviews,
            "ghost",
            cols,
        );
        assert_eq!(ghost, *layout.review_line.values().min().unwrap());
        assert_eq!(commit_review_offset("abc1234", "s", "b", &[], "x", cols), 0);
    }

    #[test]
    fn render_plan_doc_titles_and_reports_missing() {
        let (lines, total) = render_plan_doc("my-plan", Some("# Heading\n\nbody"), 0, 40, 60);
        assert!(total >= 3);
        assert!(
            visible(&lines[0]).contains("MY-PLAN"),
            "stem in the rule title: {:?}",
            visible(&lines[0])
        );
        assert!(
            lines.iter().any(|l| visible(l).contains("Heading")),
            "markdown rendered into the body"
        );
        // A missing file degrades to a dim note, never a blank screen.
        let (miss, _) = render_plan_doc("gone", None, 0, 40, 60);
        assert!(
            miss.iter()
                .any(|l| visible(l).contains("plan file not found"))
        );
    }

    #[test]
    fn agent_detail_renders_toggles_red_remove_and_stable_gutter() {
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;
        let agent = agent_row("codex", RosterRole::Commit, AutoMode::On);
        let actions = detail_actions(RosterRole::Commit); // auto,tier,promote,remove,back
        // Select the auto toggle (row 0).
        let (lines, _) = render_agent_detail(&agent, &actions, 0, 24, 60);
        let texts: Vec<String> = lines.iter().map(|l| visible(l)).collect();
        let raw = lines.join("\n");

        // auto is a segmented toggle; the review tier is checkbox rows —
        // each shown ONCE (not also a read-only info line).
        let auto = texts.iter().find(|t| t.contains("auto")).expect("auto row");
        let commit_row = texts
            .iter()
            .find(|t| t.contains("commit"))
            .expect("commit checkbox row");
        assert!(
            auto.contains("on") && auto.contains("off"),
            "auto segmented: {auto:?}"
        );
        assert!(
            commit_row.contains("[x] commit"),
            "commit-tier reviewer has commit ticked: {commit_row:?}"
        );
        assert_eq!(
            texts.iter().filter(|t| t.contains("auto")).count(),
            1,
            "auto once"
        );
        assert_eq!(
            texts.iter().filter(|t| t.contains("commit")).count(),
            1,
            "commit checkbox once"
        );

        // auto is ON → the active "on" renders bold; the ticked checkbox
        // renders bold too.
        assert!(
            raw.contains("\x1b[1mon\x1b[0m"),
            "active option bold: {raw:?}"
        );
        assert!(
            raw.contains("\x1b[1m[x] commit\x1b[0m"),
            "ticked checkbox bold: {raw:?}"
        );
        // Selected row gets the ▸ caret; the gutter is a fixed 2 DISPLAY
        // columns (▸ is one column but 3 bytes), so the label column is
        // identical on selected and unselected rows — no horizontal jitter
        // as the cursor moves.
        let gutter = |row: &str, label: &str| display_width(&row[..row.find(label).unwrap()]);
        assert_eq!(gutter(auto, "auto"), 2, "selected caret gutter is 2 cols");
        assert_eq!(
            gutter(commit_row, "reviews"),
            2,
            "unselected gutter is the same 2 cols"
        );
        assert!(auto.starts_with("▸ "), "selected row caret: {auto:?}");

        // remove is destructive: ✗ glyph + red SGR.
        assert!(
            raw.contains("\x1b[31m✗ remove from team\x1b[0m"),
            "red remove with ✗: {raw:?}"
        );
    }
}
