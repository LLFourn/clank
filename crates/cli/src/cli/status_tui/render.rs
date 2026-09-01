//! The view: pure rendering of a [`StatusSnapshot`] (+ interactive
//! [`PanelView`] state) into ANSI lines. The who's-active bar, the
//! greedy-fit gauge stack, the agents panel, and the scrollable log —
//! plus the full-screen modes (the add-picker, per-agent detail page,
//! and confirmation pages). Builds on the text primitives, the derived
//! state, the scroll model, and the input mode; performs no IO.

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
    let color = state_color(snap).sgr();
    let color = color.as_str();

    // Picker/detail/confirm modes are DEDICATED full screens — they
    // replace the normal bar/gauges/log layout while open.
    if let Mode::AddPicker { sel, tier } = mode {
        return render_add_screen(picker, rows, cols, sel, tier);
    }
    if let Mode::SwapPicker { out, sel } = mode {
        // A stale `out` falls through to the panel, as the detail page
        // does; the loop's Refresh moves the mode off next tick.
        if let Some(agent) = snap.agents.get(out) {
            return render_candidate_screen(picker, rows, cols, sel, Some(&agent.label), None);
        }
    }
    // (A stale `idx` — roster shrank under us — falls through to the
    // panel; the loop's Refresh reset moves the mode off detail next tick.)
    if let Mode::AgentDetail { idx, sel } = mode
        && let Some(agent) = snap.agents.get(idx)
    {
        let actions = detail_actions(agent.role);
        return render_agent_detail(agent, &actions, sel, rows, cols);
    }
    if let Mode::WaitDetail { agent, sel } = mode
        && let Some((a, att)) = snap
            .agents
            .get(agent)
            .and_then(|a| a.attending.as_ref().map(|t| (a, t)))
    {
        let actions = wait_actions(att.killable_pid().is_some());
        return render_wait_page(&a.label, att, &actions, sel, rows, cols);
    }
    if let Mode::Confirm { action } = mode {
        match action {
            ConfirmAction::AddCandidate { .. } | ConfirmAction::RemoveAgent { .. } => {
                return render_roster_confirm(snap, picker, action, rows, cols);
            }
            ConfirmAction::KillAttended => {
                if let Some((a, att)) = view
                    .wait_page
                    .and_then(|p| {
                        snap.agents
                            .iter()
                            .find(|a| a.label == p.label)
                            .map(|a| (a, a.attending.as_ref()))
                    })
                    .and_then(|(a, att)| att.map(|t| (a, t)))
                {
                    return render_kill_confirm(&a.label, att, rows, cols);
                }
                return (vec![region_rule("stop", "", true, cols)], 1);
            }
            ConfirmAction::StashPlan | ConfirmAction::PurgeArtifacts | ConfirmAction::PurgeDrop => {
                if let Mode::BlockAnswer { block } = mode {
                    if let Some(b) = snap.blocks.get(block) {
                        return render_block_answer(
                            &b.agent,
                            &b.name,
                            &b.question,
                            view.plan_input.unwrap_or(&TextInput::default()),
                            rows,
                            cols,
                        );
                    }
                }
                if matches!(mode, Mode::PauseInput) {
                    return render_pause_input(
                        view.plan_input.unwrap_or(&TextInput::default()),
                        rows,
                        cols,
                    );
                }
                if let Some(pp) = view.plan_page {
                    return render_plan_confirm(&pp.stem, action, rows, cols);
                }
            }
        }
    }
    // The github event page (tui-github-event-page): a dedicated
    // full screen like the other detail pages. A missing
    // `event_page` (invariant breach) falls through; the loop bails
    // the mode out next key.
    if let Some(ep) = view.event_page
        && let Mode::EventDetail { sel } = mode
    {
        return render_event_detail(ep, &super::input::actions_for(ep), sel, rows, cols);
    }
    // The plan-actions page family (tui-plan-actions-page): page and
    // purge chooser are dedicated full screens too.
    // A missing `plan_page` (invariant breach) falls through to the
    // normal layout; the loop bails the mode out next key.
    if let Some(pp) = view.plan_page {
        match mode {
            Mode::PlanDetail { sel } => {
                return render_plan_detail(pp, &plan_actions(pp.st), sel, rows, cols);
            }
            Mode::PurgeChoice { sel } => {
                return render_purge_choice(&pp.stem, sel, rows, cols);
            }
            Mode::PlanInput { kind } => {
                return render_plan_input(
                    &pp.stem,
                    kind,
                    view.plan_input.unwrap_or(&TextInput::default()),
                    rows,
                    cols,
                );
            }
            _ => {}
        }
    }

    let mut out: Vec<String> = Vec::with_capacity(rows);
    out.push(bar(snap, color, cols));

    // The SCROLLABLE header (everything between the pinned bar and the
    // log region), built in FULL by [`scrollable_header`] so the loop
    // can measure it for the pressure-lift policy, then composed with
    // the first `view.lift` rows skipped (tui-short-pane-whole-scroll).
    // At lift == 0 the compose-time row cap reproduces the old
    // per-section greedy cuts exactly.
    let head_out = scrollable_header(snap, rows as u16, cols as u16, frame, view);
    let head_len = head_out.len();

    // Compose: pinned bar + the scrollable header with the first
    // `lift` rows scrolled off, capped to the pane
    // (tui-short-pane-whole-scroll).
    let lift = view.lift.min(head_len);
    for line in head_out.into_iter().skip(lift) {
        if out.len() >= rows {
            break;
        }
        out.push(line);
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
    // The log is a focusable region ONLY when there's an agents panel to
    // switch focus with — so a panel-less render keeps the bare log
    // (no rule), unchanged.
    let has_panel = !snap.agents.is_empty();
    let log_focused = mode.log_focused();
    // Render the log region when it has content OR when there's a panel
    // to switch focus with (so both focusable regions, and which one is
    // live, stay visible even with an empty log). When a panel is
    // present the LOG rule is also the breaker between panel and log.
    // The region's row budget comes from THE shared arithmetic
    // ([`log_budget`]) — render draws its decision; the loop settles
    // the log window against the same numbers.
    let budget = log_budget(rows, head_len, lift, has_panel, log_focused, total);
    let log_capacity = budget.capacity;
    {
        match budget.divider {
            Divider::Rule => {
                // Lift on scroll (Material app-bar elevation): the flat rule
                // while the log is at its top; the same bar on a raised
                // surface while entries are scrolled UNDER it. The bar
                // settling flat is also the cue that the next Up crosses
                // into the panel (panel-focus-tops-log invariant). The
                // rule carries the BRANCH as its note — the branch lives
                // where the commits are (tui-gauges-declutter).
                let branch = snap.branch.as_deref().unwrap_or("");
                let clipped = offset.min(total.saturating_sub(1));
                out.push(if clipped > 0 {
                    region_rule_elevated_with_note("log", branch, "↑↓ scroll", log_focused, cols)
                } else {
                    region_rule_with_note("log", branch, "↑↓ scroll", log_focused, cols)
                });
            }
            Divider::Separator => out.push(String::new()),
            Divider::None => {}
        }
        let avail = budget.capacity;
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
                    Seg::Ask(a) => a.spans.clone(),
                    Seg::Log(row) => log_row_spans(row, author_width, &snap.log_decorations),
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

/// The FULL scrollable header (everything between the pinned bar and
/// the log region): gauge body, AGENTS, STASH and QUEUE sections —
/// unclipped, so the caller can both compose it (render_at) and
/// MEASURE it (the pressure-lift policy needs the true length; the
/// clipped render under-reports overflow — codex be2b054).
pub(super) fn scrollable_header(
    snap: &StatusSnapshot,
    rows: u16,
    cols: u16,
    frame: usize,
    view: &PanelView,
) -> Vec<String> {
    let mode = view.mode;
    let rows = rows.max(1) as usize;
    let cols = cols.max(1) as usize;
    let color = state_color(snap).sgr();
    let color = color.as_str();
    let mut head_out: Vec<String> = Vec::new();

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

    // `git` — branch + head, dim: PANEL-LESS renders only. A panel
    // render carries the branch on the LOG rule and the head as the
    // log's newest row — this line would echo both
    // (tui-gauges-declutter). When the worktree is dirty the stats get
    // their OWN line below, with GitHub-colored counts
    // (tui-dirty-line-color).
    if snap.agents.is_empty() {
        let branch = snap.branch.as_deref().unwrap_or("?");
        let head = snap.head_sha.as_deref().map(short_sha).unwrap_or("?");
        body.push(vec![label("git"), dim(format!("{branch} {head}"))]);
    }
    if let Some(d) = &snap.dirty {
        body.push(dirty_spans(d));
    }

    // `done` — last finished, only when the repo is idle: PANEL-LESS
    // renders only (the finalize row in the log carries it —
    // tui-gauges-declutter).
    if snap.plans.is_empty()
        && snap.agents.is_empty()
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

    // Breathing room under the bar: PANEL-LESS only — panel renders
    // put the agent rows DIRECTLY under the bar (tui-gauges-declutter,
    // codex bef2d5c), keeping panel-less bytes unchanged.
    if breath && snap.agents.is_empty() && !body.is_empty() {
        head_out.push(String::new());
    }

    // AGENTS — the roster leads the header, directly under the bar and
    // WITHOUT a title rule: everyone knows what the rows are, the
    // selection band shows focus, and the detail page teaches the keys
    // (tui-gauges-declutter). The armed auto-mode reads as ▶ play /
    // ⏸ pause (the EFFECTIVE mode governing each agent's NEXT
    // Stop-hook decision — NOT a live run/stop indicator). Gated on a
    // non-empty roster so a teamless repo (and every panel-less
    // render) is byte-for-byte unchanged.
    let wait_view = agent_wait_view(snap, cols);
    // The SAME enumeration the cursor walks: deriving positions
    // arithmetically here is what let the queue rows highlight at
    // the stash rows' offsets.
    let panel = panel_rows(
        snap.agents.len(),
        &wait_view
            .iter()
            .enumerate()
            .filter_map(|(i, (_, m))| m.is_some().then_some(i))
            .collect::<Vec<_>>(),
        snap.stash.len(),
        snap.queue.len(),
    );
    let at = |row: PanelRow| panel.iter().position(|r| *r == row);
    if !snap.agents.is_empty() {
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
            let mut spans = vec![
                auto_mark(a.auto_mode),
                plain(format!(" {}", a.label)),
                dim(format!("  {}", tier_label(a.role))),
            ];
            // An attending agent is BLOCKED, not working, so the
            // hourglass replaces the spinner and its verb rather than
            // crowding in beside them — `⌛ working…` claims two things
            // at once, and the wrong one is the animated one.
            // Two INDEPENDENT questions. Whether the agent is blocked
            // is about the record; whether its wait fits on screen is
            // about the pane. Deriving the first from the second put a
            // spinning `working…` on an agent that was blocked, merely
            // because the pane was too narrow to say on what (codex on
            // 46e5e53).
            let (waiting, marker) = wait_view
                .get(i)
                .map(|(w, m)| (*w, m.clone()))
                .unwrap_or((false, None));
            // No live wait — including a record whose process has
            // ended, which leaves the agent free to be working.
            if !waiting {
                if let Some(verb) = verb_for(&a.label) {
                    spans.push(dim(format!("  {}", spinner_glyph(frame))));
                    spans.push(italic(format!(" {verb}…")));
                }
            }
            head_out.push(row_line(
                &spans,
                mode.selected() == at(PanelRow::Agent(i)),
                color,
                cols,
            ));
            // The wait gets its OWN line beneath its agent, so naming
            // it costs the agent row nothing. Not selectable: rows stay
            // 1:1 with the cursor's agent indices, and reaching this
            // line with the cursor is a separate plan.
            if let Some(marker) = marker {
                head_out.push(row_line(
                    &[dim(format!("{ATTENDING_INDENT}{marker}"))],
                    mode.selected() == at(PanelRow::Wait(i)),
                    color,
                    cols,
                ));
            }
        }
        // "+ add" button — the last selectable row (cursor index
        // `agents.len()`).
        {
            let add_selected = mode.selected() == at(PanelRow::Add);
            let spans = vec![plain("+ add agent".to_string())];
            head_out.push(row_line(&spans, add_selected, color, cols));
        }
    }

    // The gauge body — the load-bearing state the log does NOT carry
    // (gate / fix / pr / dirty; git + done are panel-less-only above).
    for line in body {
        head_out.push(emit(&line, color, cols));
    }

    // STASH — stashed plans ABOVE the queue (they have commits, nearer
    // to live work than queued ideas). Selection continues from the
    // panel: STASH rows at `agents.len()+1..`, QUEUE rows after them.
    // Enter reads the plan (body from the record's protective ref), `o`
    // opens its HTML page. Hidden when empty; panel-less renders keep
    // the gauge summary instead.
    if !snap.agents.is_empty() && !snap.stash.is_empty() {
        let agents_focused = mode.agents_focused();
        head_out.push(region_rule(
            "stash",
            "⏎ read · o page",
            agents_focused,
            cols,
        ));
        for (i, item) in snap.stash.iter().enumerate() {
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
            head_out.push(row_line(
                &spans,
                mode.selected() == at(PanelRow::Stash(i)),
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
    if !snap.agents.is_empty() && !snap.queue.is_empty() {
        let agents_focused = mode.agents_focused();
        head_out.push(region_rule(
            "queue",
            "⏎ read · o page · +/- priority",
            agents_focused,
            cols,
        ));
        for (i, item) in snap.queue.iter().enumerate() {
            let spans = vec![
                plain("  ".to_string()),
                dim(format!("{:03} ", item.priority)),
                plain(item.name.clone()),
            ];
            head_out.push(row_line(
                &spans,
                mode.selected() == at(PanelRow::Queue(i)),
                color,
                cols,
            ));
        }
    }

    head_out
}

/// Newest-at-top render with no scroll — the common case the layout
/// tests exercise. Test-only; the live loop calls [`render_at`] directly.
#[cfg(test)]
pub(super) fn render(snap: &StatusSnapshot, rows: u16, cols: u16) -> Vec<String> {
    render_at(snap, rows, cols, 0, 0, &PanelView::just(Mode::LogScroll)).0
}

/// Per agent: is it BLOCKED on a wait, and what marker (if any) fits
/// beside it at `cols`.
///
/// One source for both facts, because the renderer and the cursor must
/// not disagree: a row the cursor can reach has to be a row the user
/// can see, and a blocked agent must not spin merely because its wait
/// did not fit.
pub(super) fn agent_wait_view(snap: &StatusSnapshot, cols: usize) -> Vec<(bool, Option<String>)> {
    let now = time::OffsetDateTime::now_utc();
    let budget = cols.saturating_sub(ATTENDING_INDENT.len());
    snap.agents
        .iter()
        .map(|a| {
            let fields = a.attending.as_ref().and_then(|att| att.marker_fields(now));
            let marker = fields.as_ref().and_then(|f| fit_marker(f, budget));
            (fields.is_some(), marker)
        })
        .collect()
}

/// The agent indices whose wait the cursor may land on.
pub(super) fn drawable_waits(snap: &StatusSnapshot, cols: usize) -> Vec<usize> {
    agent_wait_view(snap, cols)
        .into_iter()
        .enumerate()
        .filter_map(|(i, (_, marker))| marker.is_some().then_some(i))
        .collect()
}

/// The attendance line sits one step in from its agent. The agents
/// list used to carry this indent for nothing; it is spent here, where
/// an indent actually means something.
const ATTENDING_INDENT: &str = "  ";

/// Fit the attendance marker to `budget` display columns.
///
/// Fields drop from the RIGHT — pid, then age — so a narrowing line
/// only gets shorter. The SUBJECT never drops: it is the whole point
/// of the marker, and a marker that cannot name what it attends is
/// not a smaller marker, it is the bug this exists to prevent
/// (`⌛ 2m` said waiting, for two minutes, on nothing it would name).
///
/// Below `MIN_SUBJECT` columns of subject text it returns None rather
/// than an ellipsis naming nothing: `⌛ t…` is the bare hourglass in
/// disguise. A pane too narrow to say what a wait is says nothing,
/// and the agent row above it is still there.
fn fit_marker(fields: &crate::cli::stop_hook::MarkerFields<'_>, budget: usize) -> Option<String> {
    /// Fewer columns than this names nothing worth reading.
    const MIN_SUBJECT: usize = 6;
    const GLYPH: &str = "⌛ ";

    let head = display_width(GLYPH);
    // A subject is drawn into a terminal row, so it must BE one row:
    // `one_line` takes the first line and drops control bytes, which
    // would otherwise inject extra rows or escape sequences from a
    // `--desc` (or a hand-edited record). If nothing legible survives
    // — including a persisted task that was empty all along — there is
    // nothing to name and so no marker (codex on 46e5e53).
    let subject = one_line(fields.subject, budget);
    let subject = subject.trim();
    if subject.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = vec![subject.to_string()];
    if let Some(age) = &fields.age {
        parts.push(age.clone());
    }
    if let Some(pid) = fields.pid {
        parts.push(format!("pid {pid}"));
    }
    // Drop from the right while the whole thing overflows.
    while parts.len() > 1 && head + display_width(&parts.join(" · ")) > budget {
        parts.pop();
    }
    let joined = parts.join(" · ");
    if head + display_width(&joined) <= budget {
        return Some(format!("{GLYPH}{joined}"));
    }

    // Subject alone still overflows: truncate it, but never past the
    // point where it identifies anything.
    let room = budget.saturating_sub(head);
    let cut = truncate_to(subject, room);
    let kept = display_width(&cut).saturating_sub(if cut.ends_with('…') { 1 } else { 0 });
    (kept >= MIN_SUBJECT).then(|| format!("{GLYPH}{cut}"))
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
pub(super) fn log_row_spans(
    row: &crate::cli::log::OnelineRow,
    author_width: usize,
    decorations: &std::collections::BTreeMap<String, Vec<String>>,
) -> Vec<Span> {
    use crate::cli::log::{OnelineRow, RowMarker};
    match row {
        OnelineRow::Header { plan } => {
            vec![Span(
                Style::Highlight,
                plan.clone().unwrap_or_else(|| "adhoc".to_string()),
            )]
        }
        // A merged github event in the timeline: cyan `gh` badge, the
        // shared describe line, and the open-work mark while any
        // agent's copy is unhandled (log-timeline-github-events).
        OnelineRow::Github {
            event_idx: _,
            line,
            unhandled,
            baseline,
        } => {
            // Pre-watch history renders fully DIM — it's context, not
            // team activity (github-watch-resilience).
            let mut spans = if *baseline {
                vec![dim(format!("gh {line} (pre-watch)"))]
            } else {
                vec![colored("36", "gh".to_string()), plain(format!(" {line}"))]
            };
            if *unhandled {
                spans.push(colored("33", " ⚠".to_string()));
            }
            spans
        }
        OnelineRow::Notice(n) => vec![dim(format!("({n})"))],
        OnelineRow::PlainCommit { sha, subject, refs } => {
            let mut spans = vec![
                plain(" ".to_string()),
                dim(format!(" {} ", &sha.as_str()[..7])),
                plain(subject.clone()),
            ];
            if !refs.is_empty() {
                spans.push(dim(format!(" ({})", refs.join(", "))));
            }
            spans
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
            let mut spans = vec![
                icon,
                dim(format!(" {} ", &sha.as_str()[..7])),
                plain(subject.clone()),
            ];
            // Branch tips on this commit (tui-log-branch-decorations):
            // git-decorate colors — local branches green, remote-tracking
            // red — inside dim parens so the names read as annotation,
            // not subject text. Row-level width truncation handles
            // narrow panes.
            if let Some(names) = decorations.get(sha.as_str()) {
                spans.push(dim(" ("));
                for (i, name) in names.iter().enumerate() {
                    if i > 0 {
                        spans.push(dim(", "));
                    }
                    let code = if name.contains('/') { "31" } else { "32" };
                    spans.push(colored(code, name.clone()));
                }
                spans.push(dim(")"));
            }
            spans
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

/// One rendered line of a pending block's question, tagged with the
/// block it came from so the cursor can answer THAT block. A question
/// wraps to several lines; every one carries the same index.
#[derive(Clone)]
pub(super) struct AskLine {
    /// The block this line belongs to, or `None` for the trailing key
    /// hint — which must not be answerable, or `u` on it would answer
    /// whichever block happened to be last.
    pub block: Option<usize>,
    pub spans: Vec<Span>,
}

/// Full-width wrapped lines for every pending (unanswered) block ask, in
/// accent style with NO label or gutter: the accent (red) already marks it
/// as a block, and a human question often needs the whole pane. Rendered as
/// SCROLLABLE content so a long question can be paged. Pure (word-wrap
/// only): safe to call on an animation tick. Empty when no ask is pending
/// (reserves no space).
pub(super) fn block_ask_spans(snap: &StatusSnapshot, cols: usize) -> Vec<AskLine> {
    let width = cols.max(1);
    let mut lines = Vec::new();
    for (block, b) in snap
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.answer.is_none())
    {
        for line in wrap(b.question.trim(), width) {
            lines.push(AskLine {
                block: Some(block),
                spans: vec![accent(line)],
            });
        }
    }
    if !lines.is_empty() {
        lines.push(AskLine {
            block: None,
            spans: vec![dim("  ↑↓ select · u answer".to_string())],
        });
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
        // Ad-hoc ChangesRequested is an actionable MASTER item
        // (AdHocRevise) that wait returns BEFORE scanning the queue —
        // revising beats promote (codex 0c31c92). Head correction
        // owns the bar over it.
        [] if snap.head_correction.is_none()
            && snap
                .ad_hoc
                .iter()
                .any(|a| a.gate == clank_core::vocab::CommitGateState::ChangesRequested) =>
        {
            let ah = snap
                .ad_hoc
                .iter()
                .find(|a| a.gate == clank_core::vocab::CommitGateState::ChangesRequested)
                .expect("guard checked");
            (
                format!(
                    "🔨 {} revising",
                    snap.master.as_deref().unwrap_or("master").to_uppercase()
                ),
                crate::cli::status::short_sha(ah.sha.as_str()).to_string(),
            )
        }
        [] if !snap.queue.is_empty() => {
            let master = snap.master.as_deref().unwrap_or("master").to_uppercase();
            // Mirror the wait promote scan (wait-ignores-queue-only-blocks):
            // a queued plan is promotable only if no PENDING block
            // suppresses it — a repo-wide block (plan = None) suppresses
            // ALL, a plan-scoped one suppresses its plan. Block > promote:
            // if nothing is promotable the queue is STOPPED on a human ask,
            // so show the block lamp (🙋), not the promote lamp.
            // Blocks are repo-wide: one pending block stops the queue
            // regardless of which item is next.
            let pending = |_name: &str| snap.blocks.iter().any(|b| b.answer.is_none());
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
        // Unreviewed ad-hoc: the reviewers' turn — BELOW the queue
        // arm, because an Unreviewed ad-hoc gives master nothing and
        // wait does reach queue promotion (codex 0c31c92). Terminal /
        // unrouted gates fall through; head correction owns the bar.
        [] if snap.head_correction.is_none() && snap.ad_hoc.iter().any(|a| a.is_open()) => {
            let ah = snap
                .ad_hoc
                .iter()
                .find(|a| a.is_open())
                .expect("guard checked");
            match super::derive::awaited_reviewers(snap).first() {
                Some(reviewer) => (
                    format!("👀 {} reviewing", reviewer.to_uppercase()),
                    crate::cli::status::short_sha(ah.sha.as_str()).to_string(),
                ),
                // Roster gap: nobody CAN act — say so honestly.
                None => ("💤 idle".to_string(), String::new()),
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
    tier: crate::cli::teams_config::ReviewKind,
) -> (Vec<String>, usize) {
    render_candidate_screen(picker, rows, cols, sel, None, Some(tier))
}

/// The candidate list, framed by what choosing one will DO.
///
/// One screen for both callers because the candidate set is the same
/// question — the library minus this roster — and a second list would
/// drift from the first.
pub(super) fn render_candidate_screen(
    picker: &[crate::cli::status::AvailableAgent],
    rows: usize,
    cols: usize,
    sel: usize,
    swapping_out: Option<&str>,
    // `None` when swapping: the incoming agent takes the outgoing
    // one's place, so there is no tier to choose here.
    tier: Option<crate::cli::teams_config::ReviewKind>,
) -> (Vec<String>, usize) {
    let (title, verb) = match swapping_out {
        Some(out) => (format!("swap out {out}"), "swap in"),
        None => ("add a reviewer".to_string(), "add"),
    };
    let mut out: Vec<String> = Vec::new();
    out.push(region_rule(&title, "", true, cols));
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
        let tier_hint = match tier {
            Some(t) => format!(" · ←→ tier: {}", tier_label(t.into())),
            None => String::new(),
        };
        out.push(emit(
            &[dim(format!("  ↑↓ move{tier_hint} · ⏎ {verb} · Esc cancel"))],
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
        // No `…`: that is this UI's TRUNCATION marker, and
        // `detail_page_invocation_wraps_in_full_without_ellipsis` reads
        // its presence as a truncated invocation. `+ add agent` sets
        // the convention for a picker-opening row — a bare label.
        DetailAction::Swap => spans.push(plain("⇄ swap for another".to_string())),
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
/// The WAIT page: what the wait is, and whether it can be stopped.
///
/// States what is NOT known rather than blanking a field — a record
/// with no pid cannot report liveness, and saying so is the honest
/// answer that "cannot check" is not "ended".
pub(super) fn render_wait_page(
    label: &str,
    att: &crate::cli::stop_hook::Attended,
    actions: &[WaitAction],
    sel: usize,
    rows: usize,
    cols: usize,
) -> (Vec<String>, usize) {
    let mut out: Vec<String> = Vec::new();
    out.push(region_rule(&format!("wait · {label}"), "", true, cols));
    out.push(String::new());

    let field = |name: &str, value: String| {
        emit(
            &[dim(format!("   {name:<11}")), plain(one_line(&value, cols))],
            "",
            cols,
        )
    };
    out.push(field("doing", att.subject().to_string()));
    out.push(field("task", att.task.clone()));
    out.push(field(
        "started",
        att.age(time::OffsetDateTime::now_utc())
            .map(|a| format!("{a} ago"))
            .unwrap_or_else(|| "unknown".to_string()),
    ));
    out.push(field(
        "process",
        match (att.pid, att.process_alive()) {
            (Some(pid), Some(true)) => format!("pid {pid}, running"),
            (Some(pid), Some(false)) => format!("pid {pid}, ended"),
            (Some(pid), None) => format!("pid {pid}"),
            (None, _) => "not recorded".to_string(),
        },
    ));

    // Why the kill row is absent, when it is. The page says it rather
    // than showing a control that cannot work.
    if let Some(why) = att.kill_blocker() {
        out.push(String::new());
        for line in wrap(
            &format!("cannot stop it: {why}"),
            cols.saturating_sub(3).max(1),
        ) {
            out.push(emit(&[dim(format!("   {line}"))], "", cols));
        }
    }

    out.push(String::new());
    for (i, a) in actions.iter().enumerate() {
        let (glyph, text, danger) = match a {
            WaitAction::Kill => ("✗", "stop this process", true),
            WaitAction::Back => ("‹", "back", false),
        };
        let spans = vec![if danger {
            colored("31", format!("   {glyph} {text}"))
        } else {
            plain(format!("   {glyph} {text}"))
        }];
        out.push(row_line(&spans, sel == i, "", cols));
    }
    let len = out.len();
    out.truncate(rows);
    (out, len)
}

/// The kill confirm. Names the target AND the blast radius: signalling
/// one pid does not reach its children, and promising a clean tree
/// kill would be a lie.
pub(super) fn render_kill_confirm(
    label: &str,
    att: &crate::cli::stop_hook::Attended,
    rows: usize,
    cols: usize,
) -> (Vec<String>, usize) {
    let mut out: Vec<String> = Vec::new();
    out.push(region_rule(&format!("stop · {label}"), "", true, cols));
    out.push(String::new());
    let pid = att.pid.map(|p| p.to_string()).unwrap_or_default();
    let body = format!(
        "Send SIGTERM to pid {pid} ({})? Its children are NOT signalled and may keep \
         running. The attendance record is kept either way.",
        att.subject()
    );
    for line in wrap(&body, cols.saturating_sub(3).max(1)) {
        out.push(emit(&[plain(format!("   {line}"))], "", cols));
    }
    out.push(String::new());
    out.push(emit(
        &[dim("   y stop it   ·   n cancel".to_string())],
        "",
        cols,
    ));
    let len = out.len();
    out.truncate(rows);
    (out, len)
}

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
/// Truncate a path to `max` display chars keeping the FILENAME (the
/// discriminating end) — `crates/…/render.rs`, never `crates/cli/s…`.
fn middle_truncate_path(path: &str, max: usize) -> String {
    if path.chars().count() <= max {
        return path.to_string();
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    let keep_head = max.saturating_sub(file.chars().count() + 2);
    let head: String = path.chars().take(keep_head).collect();
    format!("{head}…/{file}")
}

pub(super) struct CommitLayout {
    pub(super) lines: Vec<String>,
    /// First content line (the verdict-mark row) of each reviewer's block.
    pub(super) review_line: std::collections::BTreeMap<String, usize>,
}

/// Everything the commit overlay renders, borrowed from
/// [`super::CommitDetail`] — ONE bundle so the layout builder, the
/// windowing renderer, and the review-offset derivation can't drift
/// apart in their inputs.
pub(super) struct CommitDoc<'a> {
    pub(super) short_sha: &'a str,
    pub(super) subject: &'a str,
    pub(super) body: &'a str,
    pub(super) stats: &'a [crate::git_io::FileStat],
    pub(super) reviews: &'a [(String, clank_core::vocab::Verdict, String)],
}

pub(super) fn build_commit_lines(doc: &CommitDoc<'_>, cols: usize) -> CommitLayout {
    let CommitDoc {
        short_sha,
        subject,
        body,
        stats,
        reviews,
    } = *doc;
    let mut lines: Vec<String> = Vec::new();
    let mut review_line = std::collections::BTreeMap::new();
    // The rule title is upper-cased, so keep the (lowercase) sha out of it
    // and on its own line below.
    lines.push(region_rule(
        "commit",
        "↑↓ scroll · o browser · Esc back",
        true,
        cols,
    ));
    lines.push(String::new());
    // The hash is IDENTITY, not content: its own dim line, so the title
    // wraps flush-left at full width instead of hanging in a sha-wide
    // gutter (tui-commit-overlay-stats, lloyd).
    lines.push(emit(&[dim(short_sha.to_string())], "", cols));
    let subject_line: String = subject
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    for seg in wrap(&subject_line, cols.max(1)) {
        lines.push(emit(&[plain(seg)], "", cols));
    }
    // The SHAPE of the change before the prose about it: per-file
    // +/− (binary → `bin`), then a dim summary line.
    if !stats.is_empty() {
        lines.push(String::new());
        let (mut ta, mut tr) = (0u64, 0u64);
        for st in stats {
            let counts = match (st.added, st.removed) {
                (Some(a), Some(r)) => {
                    ta += a;
                    tr += r;
                    vec![
                        colored("32", format!("+{a}")),
                        plain(" ".to_string()),
                        colored("31", format!("−{r}")),
                    ]
                }
                _ => vec![dim("bin".to_string())],
            };
            let count_width: usize = 12;
            let path = middle_truncate_path(&st.path, cols.saturating_sub(count_width + 3).max(8));
            let mut spans = vec![plain(format!("  {path}  "))];
            spans.extend(counts);
            lines.push(emit(&spans, "", cols));
        }
        lines.push(emit(
            &[dim(format!(
                "  {} file{} · +{ta} −{tr}",
                stats.len(),
                if stats.len() == 1 { "" } else { "s" },
            ))],
            "",
            cols,
        ));
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
    doc: &CommitDoc<'_>,
    offset: usize,
    rows: usize,
    cols: usize,
) -> (Vec<String>, usize) {
    let layout = build_commit_lines(doc, cols);
    let total = layout.lines.len();
    let off = offset.min(total.saturating_sub(1));
    let windowed = layout.lines.into_iter().skip(off).take(rows).collect();
    (windowed, total)
}

/// The scroll offset that brings reviewer `author`'s feedback to the top
/// of the commit-detail view (its block's first line), falling back to
/// the topmost reviewer block, then 0. Derived from the SAME layout the
/// view windows, so the focus can't drift from what's drawn.
pub(super) fn commit_review_offset(doc: &CommitDoc<'_>, author: &str, cols: usize) -> usize {
    let layout = build_commit_lines(doc, cols);
    layout
        .review_line
        .get(author)
        .copied()
        .or_else(|| layout.review_line.values().copied().min())
        .unwrap_or(0)
}

/// One button block of the plan-actions page: a label line plus the
/// explanation word-wrapped BENEATH it (never clipped), indented under
/// the label. Selection reverses the whole block; danger blocks render
/// red (dim red until selected). The hotkey letter renders accent so
/// the direct keys are discoverable from the buttons themselves.
fn button_block(
    key: &str,
    label: &str,
    desc: &str,
    danger: bool,
    selected: bool,
    cols: usize,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let label_spans = if danger {
        vec![colored("31", format!("  {key}  {label}"))]
    } else {
        vec![accent(format!("  {key}  ")), plain(label.to_string())]
    };
    lines.push(row_line(&label_spans, selected, "", cols));
    let indent = "     ";
    for l in wrap(desc, cols.saturating_sub(indent.len()).max(8)) {
        let spans = if danger {
            vec![colored("2;31", format!("{indent}{l}"))]
        } else {
            vec![dim(format!("{indent}{l}"))]
        };
        lines.push(row_line(&spans, selected, "", cols));
    }
    lines
}

/// The plan-actions page (tui-plan-actions-page, redesigned by
/// tui-plan-page-redesign): button blocks over one plan, then the plan
/// DOCUMENT itself beneath them — read it here, no drill-in. No danger
/// rule: purge's red is the cue, and the danger GRADIENT deepens on
/// the chooser + confirm screens. Returns the virtual content height
/// (chrome + full document) so the loop can clamp body scroll.
/// The event page's action-row copy.
fn event_action_row(a: EventAction) -> (&'static str, &'static str, &'static str) {
    // The hotkey comes from the input layer, never a literal here:
    // this row is what tells the operator the key exists, so the two
    // must be one fact (tui-event-page-hotkeys).
    let key = super::input::event_action_key(a).1;
    match a {
        EventAction::OpenBrowser => (key, "open in browser", "launch the event URL"),
        EventAction::Ack => (key, "ack", "mark every agent's copy handled"),
        EventAction::Retry => (key, "retry", "request the body from GitHub again"),
        EventAction::Back => (key, "back", "return to the log"),
    }
}

/// The github event page (tui-github-event-page): actions on top
/// (plan-page UX), then a WINDOWED details body — identity with
/// relative age, every member with per-copy transport + state, and
/// the standing prompts (attributed when they differ) — wrapped and
/// control-char-sanitized (the events-show cleanup), scrolled with
/// the plan page's document model so long multiline prompts are
/// always reachable, with the pinned key-summary row last.
pub(super) fn render_event_detail(
    ep: &super::input::EventPage,
    actions: &[EventAction],
    sel: usize,
    rows: usize,
    cols: usize,
) -> (Vec<String>, usize) {
    let ev = &ep.event;
    let clean = |s: &str| crate::cli::events::sanitize_multiline(s).replace('\n', " ");
    let mut out: Vec<String> = Vec::new();
    let state_hint = if ev.baseline {
        "pre-watch history"
    } else if ev.unhandled {
        "UNHANDLED"
    } else {
        "handled"
    };
    out.push(region_rule(
        &format!("github · {}", ev.repo),
        state_hint,
        true,
        cols,
    ));
    out.push(String::new());
    for (i, a) in actions.iter().enumerate() {
        let (key, label, desc) = event_action_row(*a);
        out.extend(button_block(key, label, desc, false, i == sel, cols));
        out.push(String::new());
    }
    let chrome = out.len() + 1;

    // The details body: built in full, windowed below.
    let mut body: Vec<String> = Vec::new();
    // The GitHub body FIRST (codex on dcc04f9). It is the reason to
    // open this page, so in a short pane it must be the thing already
    // on screen — putting it after the facts and every copy row left
    // the operator scrolling past metadata to reach the content they
    // came for.
    if let Some(slot) = &ep.content {
        use super::event_content::ContentState;
        let width = cols.saturating_sub(4).max(1);
        match &slot.state {
            ContentState::Loading => {
                body.push(emit(&[dim("  reading from github…".to_string())], "", cols));
            }
            ContentState::Unavailable(why) => {
                body.push(emit(&[dim(format!("  {why}"))], "", cols));
            }
            ContentState::Failed(why) => {
                body.push(emit(&[dim(format!("  {why}"))], "", cols));
            }
            ContentState::Ready(b) => {
                if let Some(author) = &b.author {
                    body.push(emit(&[dim(format!("  @{}", clean(author)))], "", cols));
                }
                if b.body.is_empty() {
                    body.push(emit(&[dim("  (no body)".to_string())], "", cols));
                } else {
                    // Same display-width wrap the prompts use, so wide
                    // glyphs stay reachable through the scroll.
                    for line in crate::cli::events::sanitize_multiline(&b.body).lines() {
                        if line.is_empty() {
                            body.push(String::new());
                            continue;
                        }
                        for wrapped in super::text::wrap(line, width) {
                            body.push(emit(&[plain(format!("  {wrapped}"))], "", cols));
                        }
                    }
                }
            }
        }
    }

    let fact = |body: &mut Vec<String>, k: &str, v: String| {
        body.push(emit(
            &[dim(format!("  {k:<9}")), plain(format!(" {v}"))],
            "",
            cols,
        ));
    };
    let detail = ev
        .detail
        .as_deref()
        .map(|d| format!(" ({d})"))
        .unwrap_or_default();
    fact(&mut body, "event", format!("{}{detail}", clean(&ev.event)));
    fact(&mut body, "age", crate::cli::events::age(ev.at));
    if let Some(n) = ev.number {
        fact(&mut body, "number", format!("#{n}"));
    }
    if let Some(t) = ev.title.as_deref() {
        fact(&mut body, "title", clean(t));
    }
    if let Some(a) = ev.actor.as_deref() {
        fact(&mut body, "actor", clean(a));
    }
    if let Some(u) = ev.url.as_deref() {
        fact(&mut body, "url", clean(u));
    }
    body.push(String::new());
    body.push(emit(&[dim("  copies".to_string())], "", cols));
    for m in &ev.members {
        let state = if m.acked { "handled" } else { "UNHANDLED" };
        body.push(emit(
            &[plain(format!(
                "    {}  {}@{}  {}  {}",
                m.key.agent,
                m.key.source,
                m.key.seq,
                m.transport.as_str(),
                state
            ))],
            "",
            cols,
        ));
    }
    if !ep.prompts.is_empty() {
        body.push(String::new());
        body.push(emit(&[dim("  standing prompt".to_string())], "", cols));
        for (who, text) in &ep.prompts {
            let attributed = match who {
                Some(agent) => {
                    format!("[{agent}] {}", crate::cli::events::sanitize_multiline(text))
                }
                None => crate::cli::events::sanitize_multiline(text),
            };
            // Display-width-aware wrapping at the ACTUAL post-indent
            // width (codex bd110f0: a scalar-count chunker let emit
            // drop the tail of wide-glyph chunks — CJK text became
            // unreachable through any scroll).
            let width = cols.saturating_sub(4).max(1);
            for wrapped in super::text::wrap(&attributed, width) {
                body.push(emit(&[italic(format!("    {wrapped}"))], "", cols));
            }
        }
    }

    // Windowed exactly like the plan page's document region: clamp
    // once, elevated rule when scrolled, pinned key summary last.
    // The scroll window covers the WHOLE page — buttons included —
    // so the never-clipped contract holds structurally in any pane:
    // a short pane scrolls the chrome off the top first and every
    // body line stays reachable (codex bd110f0: the old
    // extend-then-truncate ate the body whenever the chrome filled
    // the pane). The pinned key summary keeps the last row.
    let _ = chrome;
    let mut page_lines = out;
    page_lines.push(region_rule("details", "↓/PgDn scroll", false, cols));
    page_lines.extend(body);
    let total = page_lines.len() + 1;
    // The footer owns the last row; the content window is whatever
    // remains — ZERO in a one-row pane (the footer alone shows),
    // keeping render's at-most-`rows` invariant (codex dd670c5).
    let window = rows.saturating_sub(1);
    let off = ep
        .scroll
        .min(page_lines.len().saturating_sub(window.max(1)));
    let mut out: Vec<String> = page_lines.into_iter().skip(off).take(window).collect();
    if off > 0 && !out.is_empty() {
        // The elevation cue replaces the top row while scrolled.
        out[0] = region_rule_elevated("· · ·", "scrolled", false, cols);
    }
    while out.len() < rows.saturating_sub(1) {
        out.push(String::new());
    }
    out.push(emit(
        &[dim(
            "  ↑↓ move · ⏎ select · esc/q back · space scroll".to_string()
        )],
        "",
        cols,
    ));
    (out, total)
}

pub(super) fn render_plan_detail(
    pp: &super::input::PlanPage,
    actions: &[PlanAction],
    sel: usize,
    rows: usize,
    cols: usize,
) -> (Vec<String>, usize) {
    let st = pp.st;
    let mut out: Vec<String> = Vec::new();
    let state_hint = if st.finished {
        "finished".to_string()
    } else if st.repo_paused {
        "active · repo paused".to_string()
    } else {
        "active".to_string()
    };
    out.push(region_rule(
        &format!("plan · {}", pp.stem),
        &state_hint,
        true,
        cols,
    ));
    out.push(String::new());
    for (i, a) in actions.iter().enumerate() {
        let danger = matches!(a, PlanAction::Purge);
        let (key, label, desc) = plan_action_row(*a);
        out.extend(button_block(key, label, desc, danger, i == sel, cols));
        out.push(String::new());
    }
    // The document rule is pushed AFTER the clamp below so its
    // flat-vs-lifted choice keys on the same offset the window uses.
    let chrome = out.len() + 1;

    let body_lines = match pp.body.as_deref() {
        Some(md) => super::markdown::render_markdown(md, cols),
        None => vec![emit(&[dim("  (no plan document)".to_string())], "", cols)],
    };
    // +1 for the pinned hint row: the loop clamps scroll to
    // `total - rows` and the body viewport is `rows - chrome - 1`, so
    // without it the final document line would be unreachable
    // (codex 9452520).
    let total = chrome + body_lines.len() + 1;
    // The document fills whatever the buttons left; hint stays pinned
    // on the last row.
    let viewport = rows.saturating_sub(chrome + 1);
    // Clamped ONCE; the window slice and the lift indicator both read
    // this value, so the bar can never disagree with the actual scroll
    // (plan-page-document-scroll-like-log).
    let off = pp
        .scroll
        .min(body_lines.len().saturating_sub(viewport.max(1)));
    // Lift on scroll (the log's app-bar elevation, reused): flat rule
    // at the document's top; the raised bar the moment lines scroll
    // under it.
    out.push(if off > 0 {
        region_rule_elevated("document", "↓/PgDn scroll", false, cols)
    } else {
        region_rule("document", "↓/PgDn scroll", false, cols)
    });
    out.extend(body_lines.into_iter().skip(off).take(viewport));

    while out.len() < rows.saturating_sub(1) {
        out.push(String::new());
    }
    out.truncate(rows.saturating_sub(1));
    out.push(emit(
        &[dim("  ↑↓ move · ⏎ select · esc back".to_string())],
        "",
        cols,
    ));
    (out, total)
}

/// The text for one plan-action button: hotkey, label, explanation.
/// Explanations say what the action DOES — never what a later screen
/// will show.
fn plan_action_row(a: PlanAction) -> (&'static str, &'static str, &'static str) {
    match a {
        PlanAction::OpenHtml => ("o", "open in browser", "the plan's html page with feedback"),
        PlanAction::Stash => (
            "s",
            "stash…",
            "set the plan's commits aside; pop later to resume where it left off",
        ),
        PlanAction::ForceFinish => (
            "f",
            "force finish…",
            "finalize NOW, bypassing the review gate",
        ),
        PlanAction::Squash => ("c", "squash…", "collapse the plan's commits into one"),
        PlanAction::Purge => (
            "p",
            "purge…",
            "remove this plan's reviews and feedback from history, or delete the plan entirely",
        ),
        PlanAction::Back => ("esc", "back", ""),
    }
}

/// The purge chooser (the danger door's second screen): artifacts-only
/// vs drop-everything as full button blocks, both consequences spelled
/// out in wrapped text — nothing clipped.
pub(super) fn render_purge_choice(
    stem: &str,
    sel: usize,
    rows: usize,
    cols: usize,
) -> (Vec<String>, usize) {
    let mut out: Vec<String> = Vec::new();
    out.push(region_rule(&format!("purge · {stem}"), "", true, cols));
    out.push(String::new());
    let choices: [(&str, &str, &str); 2] = [
        (
            "a",
            "artifacts only",
            "strip this plan's .clank files — reviews, feedback, the plan \
             document — out of history. The implementation commits stay.",
        ),
        (
            "d",
            "drop EVERYTHING",
            "delete the plan AND its implementation commits from the branch.",
        ),
    ];
    for (i, (key, label, desc)) in choices.iter().enumerate() {
        out.extend(button_block(key, label, desc, true, i == sel, cols));
        out.push(String::new());
    }
    if out.len() < rows {
        out.push(emit(
            &[dim("  ↑↓ move · ⏎ select · esc back".to_string())],
            "",
            cols,
        ));
    }
    out.truncate(rows);
    (out, 0)
}

/// A roster confirm page: add/remove confirmations are clear full-screen
/// pages, not inline prompts under the main agents panel. The TUI is
/// confirming a local roster edit, so the config-file consequence is
/// named directly.
pub(super) fn render_roster_confirm(
    snap: &StatusSnapshot,
    picker: &[crate::cli::status::AvailableAgent],
    action: ConfirmAction,
    rows: usize,
    cols: usize,
) -> (Vec<String>, usize) {
    let target = match action {
        // Rendered by `render_kill_confirm`, which needs the wait
        // record rather than the roster.
        ConfirmAction::KillAttended => return (Vec::new(), 0),
        ConfirmAction::AddCandidate { idx, tier } => picker
            .get(idx)
            .map(|c| format!("{} [{}] as {}", c.label, c.tool, tier_label(tier.into())))
            .unwrap_or_else(|| "?".to_string()),
        ConfirmAction::RemoveAgent { idx } => snap
            .agents
            .get(idx)
            .map(|a| format!("{} [{}]", a.label, a.tool))
            .unwrap_or_else(|| "?".to_string()),
        ConfirmAction::StashPlan | ConfirmAction::PurgeArtifacts | ConfirmAction::PurgeDrop => {
            unreachable!("plan confirmations render through render_plan_confirm")
        }
    };
    let (title, consequence) = match action {
        ConfirmAction::KillAttended => unreachable!("returned above"),
        ConfirmAction::AddCandidate { .. } => (
            format!("add reviewer `{target}`?"),
            "adds this global-library agent to the local roster in .clank/config.json",
        ),
        ConfirmAction::RemoveAgent { .. } => (
            format!("remove reviewer `{target}`?"),
            "removes this agent from the local roster in .clank/config.json",
        ),
        ConfirmAction::StashPlan | ConfirmAction::PurgeArtifacts | ConfirmAction::PurgeDrop => {
            unreachable!("plan confirmations render through render_plan_confirm")
        }
    };
    let keys = if action.default_yes() {
        "[Y]es  [n]o  (⏎ = yes)"
    } else {
        "[y]es  [N]o  (⏎ = no)"
    };

    let mut out: Vec<String> = Vec::new();
    out.push(region_rule("confirm · agents", "", true, cols));
    out.push(String::new());
    out.push(emit(&[highlight(format!(" {title} "))], "", cols));
    out.push(String::new());
    for line in wrap(consequence, cols.saturating_sub(4).max(1)) {
        if out.len() >= rows.saturating_sub(2) {
            break;
        }
        out.push(emit(&[plain(format!("  {line}"))], "", cols));
    }
    if out.len() < rows {
        out.push(String::new());
    }
    if out.len() < rows {
        out.push(emit(&[accent(format!("  {keys}"))], "", cols));
    }
    out.truncate(rows);
    (out, 0)
}

/// A plan-page confirm: the scariest chrome in the TUI, scaled to the
/// action — a full-width red reverse banner for drop, a red banner for
/// artifacts purge, a plain highlight for stash. Default is ALWAYS No.
pub(super) fn render_plan_confirm(
    stem: &str,
    action: ConfirmAction,
    rows: usize,
    cols: usize,
) -> (Vec<String>, usize) {
    let (title, consequence) = match action {
        ConfirmAction::StashPlan => (
            format!("stash `{stem}`?"),
            "its commits are set aside on a protective ref; `clank stash pop` restores them"
                .to_string(),
        ),
        ConfirmAction::PurgeArtifacts => (
            format!("PURGE `{stem}` — rewrite history?"),
            "strips the plan's .clank/ files from history; implementation commits stay;              rewrites this branch in place"
                .to_string(),
        ),
        ConfirmAction::PurgeDrop => (
            format!("DROP `{stem}` — plan AND code vanish?"),
            "every commit attributed to the plan is dropped from history, including the              implementation. The plan body is NOT saved. This rewrites the branch."
                .to_string(),
        ),
        _ => (String::new(), String::new()),
    };
    let mut out: Vec<String> = Vec::new();
    out.push(region_rule(&format!("confirm · {stem}"), "", true, cols));
    out.push(String::new());
    let banner_color = match action {
        ConfirmAction::StashPlan => "7", // reverse, uncolored
        _ => "1;41;97",                  // bold white on red — the scary one
    };
    out.push(emit(
        &[colored(banner_color, format!(" {title} "))],
        "",
        cols,
    ));
    out.push(String::new());
    for line in wrap(&consequence, cols.saturating_sub(4).max(1)) {
        if out.len() >= rows.saturating_sub(2) {
            break;
        }
        out.push(emit(&[plain(format!("  {line}"))], "", cols));
    }
    if out.len() < rows {
        out.push(String::new());
    }
    if out.len() < rows {
        out.push(emit(
            &[accent("  [y]es  [N]o  (⏎ = no)".to_string())],
            "",
            cols,
        ));
    }
    out.truncate(rows);
    (out, 0)
}

/// The one-line plan-page text input, chrome picked by KIND. The
/// input line renders the cursor as a reverse-video cell; DropStem
/// gets the scary banner plus a LIVE armed/not-armed indicator (the
/// consequence of a match is irreversible, so the screen must show
/// the state before Enter does anything).
pub(super) fn render_plan_input(
    stem: &str,
    kind: PlanInputKind,
    input: &TextInput,
    rows: usize,
    cols: usize,
) -> (Vec<String>, usize) {
    let (title, prompt, submit_hint): (String, &str, &str) = match kind {
        PlanInputKind::ForceFinishSubject => (
            format!("force finish · {stem}"),
            "one line: WHAT this plan changed (the WHY is recorded as a gate-bypass \
             provenance note)",
            "⏎ force finish",
        ),
        PlanInputKind::SquashMessage => (
            format!("squash · {stem}"),
            "the squash commit's SUBJECT (prefilled from the finish; the WHY body \
             is kept from the finish message)",
            "⏎ squash",
        ),
        PlanInputKind::DropStem => (
            format!("DROP · {stem}"),
            "",
            "⏎ drop (armed only when the name matches)",
        ),
    };
    let mut out: Vec<String> = Vec::new();
    out.push(region_rule(&title, "", true, cols));
    out.push(String::new());
    if matches!(kind, PlanInputKind::DropStem) {
        out.push(emit(
            &[colored(
                "1;41;97",
                format!(" DROP `{stem}` — the plan AND its code vanish from history "),
            )],
            "",
            cols,
        ));
        out.push(String::new());
        out.push(emit(
            &[plain(format!(
                "  Type the plan's name (`{stem}`) to arm the drop:"
            ))],
            "",
            cols,
        ));
    } else {
        for line in wrap(prompt, cols.saturating_sub(4).max(1)) {
            out.push(emit(&[dim(format!("  {line}"))], "", cols));
        }
    }
    out.push(String::new());
    push_input_line(&mut out, input, cols);
    if matches!(kind, PlanInputKind::DropStem) {
        out.push(String::new());
        let armed = super::input::drop_armed(&input.buf, stem);
        out.push(emit(
            &[if armed {
                colored("1;31", "  ARMED — ⏎ drops the plan".to_string())
            } else {
                dim("  not armed (name doesn't match yet)".to_string())
            }],
            "",
            cols,
        ));
    }
    if out.len() < rows {
        out.push(String::new());
    }
    if out.len() < rows {
        out.push(emit(
            &[dim(format!("  {submit_hint} · esc cancel"))],
            "",
            cols,
        ));
    }
    out.truncate(rows);
    (out, 0)
}

/// The input line shared by every text-entry screen: the buffer with a
/// reverse-video cursor cell.
fn push_input_line(out: &mut Vec<String>, input: &TextInput, cols: usize) {
    let (before, at) = input.buf.split_at(input.cursor.min(input.buf.len()));
    let (cursor_cell, after) = match at.split_at(at.len().min(1)) {
        ("", _) => (" ".to_string(), ""),
        (c, rest) => (c.to_string(), rest),
    };
    out.push(emit(
        &[
            accent("  > ".to_string()),
            plain(before.to_string()),
            highlight(cursor_cell),
            plain(after.to_string()),
        ],
        "",
        cols,
    ));
}

/// The answer screen for ONE pending block: the asking agent, its
/// question in full, and the input. Showing the question while the
/// human types is the point — an answer written blind is a guess.
pub(super) fn render_block_answer(
    agent: &str,
    name: &str,
    question: &str,
    input: &TextInput,
    rows: usize,
    cols: usize,
) -> (Vec<String>, usize) {
    let mut out: Vec<String> = Vec::new();
    out.push(region_rule(
        &format!("answer · {agent}/{name}"),
        "",
        true,
        cols,
    ));
    out.push(String::new());
    for line in wrap(question.trim(), cols.saturating_sub(4).max(1)) {
        out.push(emit(&[accent(format!("  {line}"))], "", cols));
    }
    out.push(String::new());
    push_input_line(&mut out, input, cols);
    if out.len() < rows {
        out.push(String::new());
    }
    if out.len() < rows {
        out.push(emit(
            &[dim("  ⏎ answer · esc cancel".to_string())],
            "",
            cols,
        ));
    }
    out.truncate(rows);
    (out, 0)
}

/// The repo pause question screen. Carries no plan stem: a pause parks
/// the whole repo, and this screen is reachable with no plan open.
pub(super) fn render_pause_input(
    input: &TextInput,
    rows: usize,
    cols: usize,
) -> (Vec<String>, usize) {
    let mut out: Vec<String> = Vec::new();
    out.push(region_rule("pause repo", "", true, cols));
    out.push(String::new());
    for line in wrap(
        "why is the repo paused? (becomes the question the human answers; \
         every agent stays parked until it is answered)",
        cols.saturating_sub(4).max(1),
    ) {
        out.push(emit(&[dim(format!("  {line}"))], "", cols));
    }
    out.push(String::new());
    push_input_line(&mut out, input, cols);
    if out.len() < rows {
        out.push(String::new());
    }
    if out.len() < rows {
        out.push(emit(&[dim("  ⏎ pause · esc cancel".to_string())], "", cols));
    }
    out.truncate(rows);
    (out, 0)
}

/// A failed action's error, full-window and scrollable (errors surface
/// IN the TUI, not on a corrupted alt-screen stderr).
pub(super) fn render_error_doc(
    title: &str,
    message: &str,
    offset: usize,
    rows: usize,
    cols: usize,
) -> (Vec<String>, usize) {
    let mut content: Vec<String> = Vec::new();
    content.push(region_rule(&format!("✗ {title}"), "esc back", true, cols));
    content.push(String::new());
    for raw in message.lines() {
        if raw.is_empty() {
            content.push(String::new());
            continue;
        }
        for line in wrap(raw, cols.saturating_sub(2).max(1)) {
            content.push(emit(&[colored("31", format!("  {line}"))], "", cols));
        }
    }
    let total = content.len();
    let off = offset.min(total.saturating_sub(1));
    let windowed = content.into_iter().skip(off).take(rows).collect();
    (windowed, total)
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

    // ── tui-plan-actions-page: page renderers ──

    fn page(st: crate::cli::status_tui::input::PlanPageState) -> super::super::input::PlanPage {
        super::super::input::PlanPage {
            stem: "my-plan".into(),
            st,
            body: Some("# my-plan\n\nThe document body rendered inline.\n".into()),
            scroll: 0,
        }
    }

    #[test]
    fn plan_detail_renders_buttons_then_the_document() {
        let st = crate::cli::status_tui::input::PlanPageState {
            finished: false,
            multi_commit: true,
            repo_paused: false,
        };
        let pp = page(st);
        let actions = plan_actions(st);
        let (lines, total) = render_plan_detail(&pp, &actions, 1, 40, 90);
        let text = lines.join("\n");
        assert!(
            text.contains("PLAN · MY-PLAN"),
            "rule title uppercased: {text}"
        );
        assert!(text.contains("stash…"));
        assert!(text.contains("force finish…"));
        assert!(text.contains("purge…"));
        // Pause is repo state on the global `b`, never a plan-page row.
        assert!(
            !text.contains("block…") && !text.contains("unblock"),
            "{text}"
        );
        // No drill-in row and no danger rule (purge's red is the cue).
        assert!(!text.contains("read the plan"));
        assert!(!text.contains("danger"));
        // The document renders beneath the buttons.
        assert!(text.contains("DOCUMENT"), "document rule: {text}");
        assert!(
            text.contains("The document body rendered inline."),
            "plan body shown on the page: {text}"
        );
        let doc_rule = lines.iter().position(|l| l.contains("DOCUMENT")).unwrap();
        let body = lines
            .iter()
            .position(|l| l.contains("document body"))
            .unwrap();
        let purge = lines.iter().position(|l| l.contains("purge…")).unwrap();
        assert!(purge < doc_rule && doc_rule < body, "buttons, then the doc");
        assert!(total > doc_rule, "total covers chrome + document");
        // Selected block (idx 1 = stash) carries the reverse band on its
        // label AND its wrapped explanation line.
        let stash = lines.iter().position(|l| l.contains("stash…")).unwrap();
        assert!(lines[stash].contains("\x1b[7m"), "label reversed");
        assert!(
            lines[stash + 1].contains("\x1b[7m") && lines[stash + 1].contains("pop later"),
            "explanation line is inside the selection band: {:?}",
            lines[stash + 1]
        );
    }

    #[test]
    fn plan_detail_wraps_explanations_and_scrolls_the_body() {
        let st = crate::cli::status_tui::input::PlanPageState {
            finished: false,
            multi_commit: true,
            repo_paused: false,
        };
        // Narrow pane: the purge explanation must WRAP, never clip.
        let pp = page(st);
        let actions = plan_actions(st);
        let (lines, _) = render_plan_detail(&pp, &actions, 0, 40, 46);
        let text = lines.join("\n");
        assert!(text.contains("reviews and feedback"), "{text}");
        assert!(
            text.contains("delete the plan entirely"),
            "wrapped tail survives at narrow width: {text}"
        );
        assert!(!text.contains("next screen"), "no next-screen meta");
        // Scrolling advances the body while the buttons stay put.
        // (Blank-line separated: markdown flows adjacent lines into one
        // paragraph.)
        let mut scrolled = page(st);
        scrolled.body = Some("line one\n\nline two\n\nline three\n\nline four\n".into());
        let (top, _) = render_plan_detail(&scrolled, &actions, 0, 24, 90);
        scrolled.scroll = 2;
        let (moved, _) = render_plan_detail(&scrolled, &actions, 0, 24, 90);
        let purge_top = top.iter().position(|l| l.contains("purge…")).unwrap();
        let purge_moved = moved.iter().position(|l| l.contains("purge…")).unwrap();
        assert_eq!(purge_top, purge_moved, "buttons pinned while scrolling");
        assert!(top.join("\n").contains("line one"));
        assert!(
            !moved.join("\n").contains("line one"),
            "scrolled past the first body line"
        );
    }

    #[test]
    fn plan_detail_document_rule_lifts_on_scroll_keyed_on_the_clamp() {
        // The log's app-bar elevation, reused: flat rule at the
        // document's top, raised bar once lines scroll under it — and
        // the choice keys on the SAME clamped offset as the window, so
        // an over-scroll that clamps back to 0 stays FLAT
        // (plan-page-document-scroll-like-log).
        let st = crate::cli::status_tui::input::PlanPageState {
            finished: false,
            multi_commit: true,
            repo_paused: false,
        };
        let cols = 90;
        let mut pp = page(st);
        pp.body = Some("p1\n\np2\n\np3\n\np4\n\np5\n\np6\n\np7\n".into());
        let actions = plan_actions(st);
        let flat = super::region_rule("document", "↓/PgDn scroll", false, cols);
        let lifted = super::region_rule_elevated("document", "↓/PgDn scroll", false, cols);

        let (top, _) = render_plan_detail(&pp, &actions, 0, 30, cols);
        assert!(top.contains(&flat), "flat rule at the top");
        assert!(!top.contains(&lifted));

        pp.scroll = 2;
        let (moved, _) = render_plan_detail(&pp, &actions, 0, 30, cols);
        assert!(moved.contains(&lifted), "raised bar while scrolled");
        assert!(!moved.contains(&flat));

        // A tiny document that fits the viewport clamps any scroll back
        // to 0 — the bar must stay flat (keyed on the clamp, not the
        // raw request).
        pp.body = Some("just one line\n".into());
        pp.scroll = 99;
        let (clamped, _) = render_plan_detail(&pp, &actions, 0, 40, cols);
        assert!(clamped.contains(&flat), "clamped-to-top stays flat");
        assert!(!clamped.contains(&lifted));
    }

    #[test]
    fn plan_detail_document_lines_are_never_highlighted() {
        // The read-only-prose guarantee: the document has no cursor and
        // no Enter target, so no body line ever carries the selection's
        // reverse-video style — even while scrolled
        // (plan-page-document-scroll-like-log).
        let st = crate::cli::status_tui::input::PlanPageState {
            finished: false,
            multi_commit: true,
            repo_paused: false,
        };
        let mut pp = page(st);
        pp.body = Some("p1\n\np2\n\np3\n\np4\n\np5\n\np6\n\np7\n".into());
        pp.scroll = 2;
        let actions = plan_actions(st);
        // Select the LAST button (the position from which the document
        // is scrolled).
        let (lines, _) = render_plan_detail(&pp, &actions, actions.len() - 1, 30, 90);
        let doc_rule = lines
            .iter()
            .position(|l| l.contains("DOCUMENT"))
            .expect("document rule present");
        for l in &lines[doc_rule + 1..] {
            assert!(
                !l.contains("\x1b[7m"),
                "document region must never carry the selection band: {l:?}"
            );
        }
    }

    #[test]
    fn plan_detail_final_document_line_is_reachable_at_the_loop_clamp() {
        // The loop clamps body scroll to `total - rows` (mirroring the
        // overlay pattern); the returned virtual height must account for
        // the pinned hint row or the last line stays one step out of
        // reach (codex 9452520).
        let st = crate::cli::status_tui::input::PlanPageState {
            finished: false,
            multi_commit: true,
            repo_paused: false,
        };
        let mut pp = page(st);
        pp.body = Some("p1\n\np2\n\np3\n\np4\n\np5\n\nfinal-line\n".into());
        let actions = plan_actions(st);
        let rows = 24;
        let (top, total) = render_plan_detail(&pp, &actions, 0, rows, 90);
        assert!(
            !top.join("\n").contains("final-line"),
            "fixture long enough to need scrolling"
        );
        pp.scroll = total.saturating_sub(rows);
        let (bottom, _) = render_plan_detail(&pp, &actions, 0, rows, 90);
        assert!(
            bottom.join("\n").contains("final-line"),
            "the loop's max scroll reaches the document's last line: {bottom:?}"
        );
    }

    #[test]
    fn purge_choice_names_both_consequences_unclipped() {
        // Narrow width: both explanations wrap and stay fully readable.
        let (lines, _) = render_purge_choice("my-plan", 1, 24, 46);
        let text = lines.join("\n");
        assert!(text.contains("PURGE · MY-PLAN"));
        assert!(text.contains("artifacts only"));
        assert!(text.contains("drop EVERYTHING"));
        assert!(text.contains("The implementation commits stay."), "{text}");
        // The drop consequence wraps at this width — assert both halves
        // so nothing was clipped.
        assert!(
            text.contains("delete the plan AND its") && text.contains("from the branch."),
            "drop consequence fully present across wrapped lines: {text}"
        );
        assert!(!text.contains("next screen"));
    }

    #[test]
    fn drop_confirm_is_the_scariest_screen_and_defaults_no() {
        let (lines, _) = render_plan_confirm(
            "my-plan",
            crate::cli::status_tui::input::ConfirmAction::PurgeDrop,
            24,
            90,
        );
        let text = lines.join("\n");
        assert!(
            text.contains("\x1b[1;41;97m"),
            "bold white-on-red banner: {text:?}"
        );
        assert!(text.contains("DROP `my-plan`"));
        assert!(text.contains("NOT saved"));
        assert!(text.contains("[y]es  [N]o  (⏎ = no)"), "default is No");
        // Stash's confirm is deliberately NOT the red banner.
        let (calm, _) = render_plan_confirm(
            "my-plan",
            crate::cli::status_tui::input::ConfirmAction::StashPlan,
            24,
            90,
        );
        assert!(
            !calm.join("").contains("\x1b[1;41;97m"),
            "gradient: stash stays calm"
        );
    }

    #[test]
    fn drop_input_arms_only_on_the_exact_stem() {
        use crate::cli::status_tui::input::{PlanInputKind, TextInput};
        let near_miss = TextInput::prefilled("my-pla");
        let (lines, _) = render_plan_input("my-plan", PlanInputKind::DropStem, &near_miss, 24, 90);
        let text = lines.join("\n");
        assert!(text.contains("\x1b[1;41;97m"), "scary banner always on");
        assert!(text.contains("not armed"), "{text}");
        let exact = TextInput::prefilled("my-plan");
        let (lines, _) = render_plan_input("my-plan", PlanInputKind::DropStem, &exact, 24, 90);
        assert!(lines.join("\n").contains("ARMED"), "exact match arms");
        // Whitespace padding must show not-armed — and the submit gate
        // uses the SAME predicate, so what renders is what executes.
        let padded = TextInput::prefilled(" my-plan ");
        let (lines, _) = render_plan_input("my-plan", PlanInputKind::DropStem, &padded, 24, 90);
        assert!(
            lines.join("\n").contains("not armed"),
            "padding stays disarmed"
        );
    }

    #[test]
    fn plan_input_screens_name_their_purpose_and_cancel_path() {
        use crate::cli::status_tui::input::{PlanInputKind, TextInput};
        for (kind, want) in [
            (PlanInputKind::ForceFinishSubject, "FORCE FINISH · MY-PLAN"),
            (PlanInputKind::SquashMessage, "SQUASH · MY-PLAN"),
        ] {
            let (lines, _) = render_plan_input("my-plan", kind, &TextInput::default(), 24, 90);
            let text = lines.join("\n");
            assert!(text.contains(want), "{want}: {text}");
            assert!(text.contains("esc cancel"), "{text}");
        }
    }

    #[test]
    fn each_pending_block_tags_its_own_lines_and_the_hint_is_not_answerable() {
        let mut s = snap(vec![], vec![]);
        s.blocks = vec![
            crate::cli::block::BlockEntry {
                agent: "claude".into(),
                name: "a".into(),
                question: "first question".into(),
                answer: None,
            },
            crate::cli::block::BlockEntry {
                agent: "codex".into(),
                name: "b".into(),
                question: "second question".into(),
                answer: None,
            },
        ];
        let lines = block_ask_spans(&s, 60);
        let owners: Vec<Option<usize>> = lines.iter().map(|l| l.block).collect();
        assert_eq!(
            owners,
            vec![Some(0), Some(1), None],
            "each question tags its own block; the trailing hint tags none"
        );
    }

    #[test]
    fn an_answered_block_contributes_no_ask_line() {
        let mut s = snap(vec![], vec![]);
        s.blocks = vec![crate::cli::block::BlockEntry {
            agent: "claude".into(),
            name: "a".into(),
            question: "already answered".into(),
            answer: Some("yes".into()),
        }];
        assert!(block_ask_spans(&s, 60).is_empty());
    }

    #[test]
    fn ask_line_indices_address_the_snapshot_not_the_pending_subset() {
        // The index answers `snapshot.blocks[i]`, so it must count
        // answered blocks too — indexing the filtered subset would
        // write the answer under the WRONG agent's question.
        let mut s = snap(vec![], vec![]);
        s.blocks = vec![
            crate::cli::block::BlockEntry {
                agent: "claude".into(),
                name: "done".into(),
                question: "answered".into(),
                answer: Some("yes".into()),
            },
            crate::cli::block::BlockEntry {
                agent: "codex".into(),
                name: "open".into(),
                question: "still open".into(),
                answer: None,
            },
        ];
        let lines = block_ask_spans(&s, 60);
        assert_eq!(lines[0].block, Some(1), "must point at the OPEN block");
    }

    #[test]
    fn block_answer_screen_shows_the_question_being_answered() {
        use crate::cli::status_tui::input::TextInput;
        let (lines, _) = render_block_answer(
            "codex",
            "scope",
            "should this ship?",
            &TextInput::default(),
            24,
            80,
        );
        let text = lines.join("\n");
        assert!(text.to_uppercase().contains("CODEX/SCOPE"), "{text}");
        assert!(text.contains("should this ship?"), "{text}");
        assert!(text.contains("esc cancel"), "{text}");
    }

    #[test]
    fn pause_screen_names_the_repo_not_a_plan() {
        use crate::cli::status_tui::input::TextInput;
        let (lines, _) = render_pause_input(&TextInput::default(), 24, 80);
        let text = lines.join("\n");
        assert!(text.to_uppercase().contains("PAUSE REPO"), "{text}");
        assert!(text.contains("esc cancel"), "{text}");
    }

    #[test]
    fn error_doc_renders_red_and_scrolls() {
        let msg = "stash push refuses: foreign commit(s)\nline two";
        let (lines, total) = render_error_doc("stash push failed", msg, 0, 10, 80);
        let text = lines.join("\n");
        assert!(text.contains("✗ STASH PUSH FAILED"));
        assert!(text.contains("\x1b[31m"), "error body is red");
        assert!(text.contains("foreign commit"));
        assert!(total >= 4);
        // Windowing honors the offset like the other doc overlays.
        let (scrolled, _) = render_error_doc("t", msg, 2, 10, 80);
        assert!(scrolled.len() < lines.len() || !scrolled[0].contains('✗'));
    }

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
    fn log_commit_rows_carry_branch_decorations() {
        // tui-log-branch-decorations: a commit whose sha is a relevant
        // branch tip renders `(name, other)` after its subject, in map
        // order; rows whose sha owns no decoration are unchanged.
        let mut s = snap_with_log(&[]);
        let tip_sha = format!("{:0<40}", "aaaa111");
        let old_sha = format!("{:0<40}", "bbbb222");
        let row = |sha: &str, subject: &str| crate::cli::log::OnelineRow::Commit {
            sha: crate::lifecycle::CommitSha::parse(sha).unwrap(),
            subject: subject.to_string(),
            marker: crate::cli::log::RowMarker::Plain,
        };
        s.log_rows = vec![row(&tip_sha, "tip subject"), row(&old_sha, "older subject")];
        s.log_decorations = [(
            tip_sha,
            vec!["master".to_string(), "origin/master".to_string()],
        )]
        .into_iter()
        .collect();

        let lines: Vec<String> = render(&s, 16, 80).iter().map(|l| visible(l)).collect();
        let tip = lines.iter().find(|l| l.contains("tip subject")).unwrap();
        assert!(
            tip.contains("tip subject (master, origin/master)"),
            "decorated tip row: {tip}"
        );
        let older = lines.iter().find(|l| l.contains("older subject")).unwrap();
        assert!(!older.contains('('), "undecorated row stays bare: {older}");
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
    fn panel_render_leads_with_agents_and_drops_echo_gauges() {
        // tui-gauges-declutter: an idle PANEL render shows no `git` and
        // no `done` gauge lines (the log's newest row is head; its
        // finalize row is the last finish), the branch rides the LOG
        // rule case-preserved, and the agent rows lead the header with
        // no AGENTS title rule.
        use clank_core::repo_state::FinishedPlan;
        let mut s = two_agent_snap();
        s.branch = Some("Feature/Mixed".into());
        s.last_finished = Some(FinishedPlan {
            plan: PlanKey::parse("old-plan").unwrap(),
            intro: CommitSha::parse(&format!("{:0<40}", "aa")).unwrap(),
            finalized_at: CommitSha::parse(&format!("{:0<40}", "bb")).unwrap(),
        });
        s.log_rows = (0..4).map(|i| commit_row(&format!("c{i}"))).collect();
        let texts: Vec<String> = render(&s, 20, 60).iter().map(|l| visible(l)).collect();
        assert!(
            !texts.iter().any(|t| t.trim_start().starts_with("git ")),
            "no git gauge line in a panel render: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.trim_start().starts_with("done ")),
            "no done gauge line in a panel render"
        );
        assert!(
            texts.iter().all(|t| !t.contains("AGENTS")),
            "no AGENTS title rule"
        );
        // Agent rows sit DIRECTLY under the bar — exact adjacency, no
        // breath row in panel renders (codex bef2d5c).
        assert!(
            texts[1].contains("claude"),
            "row 1 is the first agent, adjacent to the bar: {:?}",
            &texts[..3]
        );
        // The branch rides the log rule, case preserved beside the
        // uppercased title.
        let log_rule = texts.iter().find(|t| t.contains("LOG")).expect("log rule");
        assert!(
            log_rule.contains("LOG · Feature/Mixed"),
            "branch on the rule, original case: {log_rule}"
        );
    }

    #[test]
    fn long_branch_never_truncates_the_focused_hint_off_the_rule() {
        // codex bef2d5c: the note is fitted around a RESERVED hint, so
        // the focused and unfocused rules stay visually distinct at any
        // width. A 60-char branch in a 40-col pane:
        use super::super::text::{region_rule, region_rule_with_note};
        let branch = "feature/very-long-branch-name-that-keeps-going-and-going-x";
        let focused = region_rule_with_note("log", branch, "↑↓ scroll", true, 40);
        assert!(
            visible(&focused).contains("↑↓ scroll"),
            "the focused hint survives the long note: {}",
            visible(&focused)
        );
        // And the truncated note still starts with real branch text.
        assert!(
            visible(&focused).contains("· feature/"),
            "the note shows its head: {}",
            visible(&focused)
        );
        let unfocused = region_rule_with_note("log", branch, "↑↓ scroll", false, 40);
        assert_ne!(
            visible(&focused),
            visible(&unfocused),
            "focused and unfocused rules stay distinct"
        );
        // Degenerate width: the rule never panics and stays one line.
        for cols in [1usize, 5, 12] {
            let r = region_rule_with_note("log", branch, "↑↓ scroll", true, cols);
            assert!(!visible(&r).contains('\n'));
        }
        let _ = region_rule("log", "↑↓ scroll", true, 40); // flat variant untouched
    }

    #[test]
    fn panel_less_render_keeps_the_git_and_done_gauges() {
        // No roster → no log rule to host the branch and no agents to
        // lead with: the git/done gauge lines stay.
        use clank_core::repo_state::FinishedPlan;
        let mut s = snap(vec![], vec![]);
        s.last_finished = Some(FinishedPlan {
            plan: PlanKey::parse("old-plan").unwrap(),
            intro: CommitSha::parse(&format!("{:0<40}", "aa")).unwrap(),
            finalized_at: CommitSha::parse(&format!("{:0<40}", "bb")).unwrap(),
        });
        let texts: Vec<String> = render(&s, 10, 60).iter().map(|l| visible(l)).collect();
        assert!(
            texts.iter().any(|t| t.trim_start().starts_with("git ")),
            "panel-less keeps the git line: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.trim_start().starts_with("done ")),
            "panel-less keeps the done line"
        );
    }

    // ── tui-short-pane-whole-scroll: pressure lift ────────────

    #[test]
    fn render_capacity_always_equals_the_budget() {
        // THE consistency property the refactor exists for: render_at
        // draws log_budget's decision, and the loop settles against the
        // same numbers — so the render's returned capacity must equal
        // the budget's across the shape space (heights, lifts, focus,
        // panel presence, log lengths).
        use super::super::scroll::log_budget;
        let mut with_panel = two_agent_snap();
        with_panel.log_rows = (0..12).map(|i| commit_row(&format!("c{i}"))).collect();
        let mut empty_log = two_agent_snap();
        empty_log.log_rows = Vec::new();
        let mut panel_less = snap(vec![], vec![]);
        panel_less.log_rows = (0..12).map(|i| commit_row(&format!("c{i}"))).collect();
        for s in [&with_panel, &empty_log, &panel_less] {
            let has_panel = !s.agents.is_empty();
            for rows in [1u16, 2, 3, 4, 7, 10, 30] {
                for lift in [0usize, 1, 3, 20] {
                    for focused in [true, false] {
                        let mode = if focused {
                            Mode::LogScroll
                        } else {
                            Mode::AgentPanel { sel: 0 }
                        };
                        let view = PanelView {
                            mode,
                            plan_page: None,
                            event_page: None,
                            wait_page: None,
                            plan_input: None,
                            picker: &[],
                            log_cursor: 0,
                            lift,
                        };
                        let header_len = scrollable_header(s, rows, 60, 0, &view).len();
                        // The log is only a focusable region with a panel.
                        let log_focused = focused && has_panel;
                        let total = {
                            let ask = block_ask_spans(s, 60);
                            build_scroll(s, &ask).len()
                        };
                        let budget = log_budget(
                            rows as usize,
                            header_len,
                            lift.min(header_len),
                            has_panel,
                            log_focused,
                            total,
                        );
                        let cap = render_at(s, rows, 60, 0, 0, &view).1;
                        assert_eq!(
                            cap, budget.capacity,
                            "rows {rows} lift {lift} focused {focused} panel {has_panel}"
                        );
                    }
                }
            }
        }
    }

    fn lift_view(cursor: usize, lift: usize) -> PanelView<'static> {
        PanelView {
            plan_page: None,
            event_page: None,
            wait_page: None,
            plan_input: None,
            mode: Mode::LogScroll,
            picker: &[],
            log_cursor: cursor,
            lift,
        }
    }

    #[test]
    fn pressure_lift_policy_boundaries() {
        use super::super::scroll::pressure_lift;
        // Unfocused: never lifts, any height, any header.
        assert_eq!(pressure_lift(false, 10, 30, 9, 12), 0);
        // Tall pane, short header: fits alongside min_log → no lift.
        assert_eq!(pressure_lift(true, 40, 7, 9, 12), 0);
        // rows==10, header 7 (the two-agent fixture shape): full header
        // + one entry coexist at the top; the lift grows with descent
        // to the min_log target and no further.
        assert_eq!(
            pressure_lift(true, 10, 7, 0, 12),
            0,
            "cursor at top → full header"
        );
        assert_eq!(
            pressure_lift(true, 10, 7, 2, 12),
            2,
            "progressive with descent"
        );
        assert_eq!(
            pressure_lift(true, 10, 7, 9, 12),
            4,
            "capped at the min_log target"
        );
        // OVER-TALL header (codex be2b054): rows==10, header 20. Even at
        // cursor-at-top one entry row is guaranteed — the header is
        // partially hidden from the start (the defined tradeoff)…
        assert_eq!(
            pressure_lift(true, 10, 20, 0, 12),
            13,
            "one entry over a full header"
        );
        // …and descent still reaches the full min_log viewport.
        assert_eq!(
            pressure_lift(true, 10, 20, 9, 12),
            17,
            "min_log despite the overflow"
        );
        // Height-conditional chrome: rows==4 → min_log 2; rows==2 → the
        // rule yields (chrome 1, min_log 1, ALL header hidden for the
        // entry); rows==1 → bar only, lift can't help.
        // rows==4: bar + rule + two entries consume the pane — the
        // whole header hides at full descent.
        assert_eq!(pressure_lift(true, 4, 7, 9, 12), 7);
        assert_eq!(pressure_lift(true, 2, 7, 9, 12), 7);
        assert_eq!(pressure_lift(true, 1, 7, 9, 12), 0);
        // EMPTY sequence (codex ce616ce): nothing to reserve a viewport
        // for — no lift, however tall the header.
        assert_eq!(
            pressure_lift(true, 10, 20, 0, 0),
            0,
            "empty timeline never lifts"
        );
        // Cursor past the exhausted tail: clamped to the real sequence,
        // so the lift equals the last-entry lift, not an overshoot.
        assert_eq!(
            pressure_lift(true, 10, 20, 50, 12),
            pressure_lift(true, 10, 20, 11, 12),
            "past-end cursor clamps to the tail"
        );
        // A sequence SHORTER than min_log bounds the target too.
        assert_eq!(
            pressure_lift(true, 10, 20, 9, 2),
            pressure_lift(true, 10, 20, 1, 2),
            "target never exceeds the entries that exist"
        );
    }

    #[test]
    fn short_pane_walk_lifts_header_progressively_and_keeps_selection_visible() {
        use super::super::scroll::pressure_lift;
        // Roster + long log in a 10-row pane: the header (breath,
        // gauges, AGENTS rule + 2 rows + add) eats most of it.
        let mut s = two_agent_snap();
        s.log_rows = (0..12).map(|i| commit_row(&format!("c{i}"))).collect();
        walk_asserting_visibility(&s, 10, 12);
        // Walking back to the top restores the full header: lift(0) is 0
        // for this fixture (header + one entry fit), so the render is
        // the untouched one.
        let header_len = scrollable_header(&s, 10, 60, 0, &lift_view(0, 0)).len();
        assert_eq!(pressure_lift(true, 10, header_len, 0, 12), 0);

        // OVER-TALL header (codex be2b054): six more agents push the
        // header past the pane. Capacity alone reads 0 at every depth;
        // the header-length policy still surfaces the log.
        let mut big = two_agent_snap();
        for i in 0..9 {
            big.agents.push(crate::cli::status_tui::fixtures::agent_row(
                &format!("extra{i}"),
                crate::cli::teams_config::RosterRole::Commit,
                clank_core::vocab::AutoMode::On,
            ));
        }
        big.log_rows = (0..12).map(|i| commit_row(&format!("c{i}"))).collect();
        let header_len = scrollable_header(&big, 10, 60, 0, &lift_view(0, 0)).len();
        assert!(header_len >= 10, "fixture premise: header exceeds the pane");
        // Even at cursor-at-top the selection is visible (one entry row
        // over a partially hidden header — the defined tradeoff).
        let lift0 = pressure_lift(true, 10, header_len, 0, 12);
        let cap0 = render_at(&big, 10, 60, 0, 0, &lift_view(0, lift0)).1;
        assert!(cap0 >= 1, "one entry row guaranteed at the top");
        walk_asserting_visibility(&big, 10, 12);
    }

    /// Emulate the loop for a cursor walk: derive the lift from the
    /// true header length, probe capacity under it, settle the window,
    /// and assert the selection is always visible with the min_log
    /// target honored once descent allows it.
    fn walk_asserting_visibility(s: &StatusSnapshot, rows: u16, total: usize) {
        use super::super::scroll::{pressure_lift, scroll_to_show};
        let header_len = scrollable_header(s, rows, 60, 0, &lift_view(0, 0)).len();
        let min_log = 5.min(rows as usize - 2);
        let mut offset = 0usize;
        let mut last_header_visible = usize::MAX;
        for cursor in 0..8 {
            let lift = pressure_lift(true, rows as usize, header_len, cursor, total);
            let (_, cap) = render_at(s, rows, 60, offset, 0, &lift_view(cursor, lift));
            offset = scroll_to_show(cursor, offset, cap, total);
            let (out, cap) = render_at(s, rows, 60, offset, 0, &lift_view(cursor, lift));
            assert!(!out.is_empty(), "bar row present");
            let target = (1 + cursor).min(min_log);
            assert!(
                cap >= target,
                "cursor {cursor}: capacity {cap} ≥ target {target}"
            );
            assert!(
                offset <= cursor && cursor < offset + cap,
                "cursor {cursor} visible in [{offset}, {})",
                offset + cap
            );
            // Header rows only recede as the cursor descends.
            let header_visible = header_len - lift.min(header_len);
            assert!(header_visible <= last_header_visible, "header only recedes");
            last_header_visible = header_visible;
        }
    }

    #[test]
    fn lift_drops_header_rows_after_the_bar_only() {
        let mut s = two_agent_snap();
        s.log_rows = (0..12).map(|i| commit_row(&format!("c{i}"))).collect();
        let base = render_at(&s, 10, 60, 0, 0, &lift_view(0, 0)).0;
        let lifted = render_at(&s, 10, 60, 0, 0, &lift_view(0, 2)).0;
        // The bar (row 0) is identical; the next rows are the base's
        // header shifted up by two.
        assert_eq!(base[0], lifted[0], "the bar is pinned");
        assert_eq!(base[3], lifted[1], "header rows lifted off the top");
        // Freed rows went to the log: capacity grew by the lift.
        let cap0 = render_at(&s, 10, 60, 0, 0, &lift_view(0, 0)).1;
        let cap2 = render_at(&s, 10, 60, 0, 0, &lift_view(0, 2)).1;
        assert_eq!(cap2, cap0 + 2);
    }

    #[test]
    fn tiny_height_degradation_is_intentional() {
        let mut s = two_agent_snap();
        s.log_rows = (0..12).map(|i| commit_row(&format!("c{i}"))).collect();
        // rows == 4 and 3, focused: bar + lifted header remainder +
        // rule + log — the rule is pinned (a row exists for it).
        for rows in [4u16, 3] {
            let (out, cap) = render_at(&s, rows, 60, 0, 0, &lift_view(0, 20));
            assert!(out.len() <= rows as usize);
            assert!(
                out.iter().any(|l| visible(l).contains("LOG")),
                "rows {rows}: the focused rule is pinned"
            );
            assert!(cap >= 1, "rows {rows}: at least one entry row");
        }
        // rows == 2, focused with content: the rule YIELDS to the entry.
        let (out, cap) = render_at(&s, 2, 60, 0, 0, &lift_view(0, 20));
        assert_eq!(cap, 1, "bar + the selected entry row");
        assert!(
            !out.iter().any(|l| visible(l).contains("LOG")),
            "the rule yields its row to the selection"
        );
        // rows == 2, UNFOCUSED: today's greedy fit unchanged — bar +
        // first header row, no pressure lift ever engages.
        let unfocused = PanelView {
            mode: Mode::AgentPanel { sel: 0 },
            ..lift_view(0, 0)
        };
        let (out, _) = render_at(&s, 2, 60, 0, 0, &unfocused);
        assert_eq!(out.len(), 2);
        // rows == 1: bar only — the one height where selection
        // visibility is unsatisfiable (no panic, no overdraw).
        let (out, cap) = render_at(&s, 1, 60, 0, 0, &lift_view(0, 20));
        assert_eq!(out.len(), 1, "bar only");
        assert_eq!(cap, 0);
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
            plan_page: None,
            event_page: None,
            wait_page: None,
            plan_input: None,
            mode: Mode::LogScroll,
            picker: &[],
            log_cursor: 6,
            lift: 0,
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
            question: "which sims?".into(),
            answer: None,
        }];
        let v = visible(&render(&s, 1, 60)[0]);
        assert!(v.starts_with("🙋 CLAUDE blocked"), "got `{v}`");
    }

    #[test]
    fn any_pending_block_stops_the_queue() {
        // Blocks are repo-wide, so a pending one halts promotion
        // outright — there is no longer a "lower unblocked item" to
        // fall through to. The lamp must say blocked, not promote,
        // or the human is invited to take work the agent will refuse.
        let mut s = snap(vec![], vec!["blocked-plan", "free-plan"]);
        s.blocks = vec![crate::cli::block::BlockEntry {
            agent: "claude".into(),
            name: "q".into(),
            question: "?".into(),
            answer: None,
        }];
        let v = visible(&render(&s, 1, 60)[0]);
        assert!(v.contains("blocked"), "expected the block lamp, got `{v}`");
        assert!(
            !v.contains("promote"),
            "must not offer promotion while blocked: `{v}`"
        );
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
                plan_page: None,
                event_page: None,
                wait_page: None,
                plan_input: None,
                mode: Mode::AddPicker {
                    sel: 0,
                    tier: crate::cli::teams_config::ReviewKind::Commit,
                },
                picker: &picker,
                log_cursor: 0,
                lift: 0,
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
            plan_page: None,
            event_page: None,
            wait_page: None,
            plan_input: None,
            mode: Mode::AddPicker {
                sel: 0,
                tier: crate::cli::teams_config::ReviewKind::Commit,
            },
            picker: &picker,
            log_cursor: 0,
            lift: 0,
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
            &PanelView::just(Mode::AddPicker {
                sel: 0,
                tier: crate::cli::teams_config::ReviewKind::Commit,
            }),
        )
        .0
        .join("\n");
        assert!(
            out.contains("no agents available") && out.contains("--global"),
            "empty picker points at `clank agent add --global`: {out}"
        );
    }

    #[test]
    fn roster_confirm_pages_name_action_consequence_and_default() {
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
        assert!(rm.contains("CONFIRM · AGENTS"), "full-screen title: {rm}");
        assert!(rm.contains("remove reviewer"), "names the action: {rm}");
        assert!(rm.contains("codex [claude]"), "names the target: {rm}");
        assert!(
            rm.contains(".clank/config.json"),
            "names the consequence: {rm}"
        );
        assert!(
            rm.contains("[N]o") && rm.contains("⏎ = no"),
            "remove default is No: {rm}"
        );
        assert!(!rm.contains("confirm:"), "old inline prompt removed: {rm}");
        assert!(
            !rm.contains("+ add agent"),
            "confirm page replaces the normal agents panel: {rm}"
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
                plan_page: None,
                event_page: None,
                wait_page: None,
                plan_input: None,
                mode: Mode::Confirm {
                    action: ConfirmAction::AddCandidate {
                        idx: 0,
                        tier: crate::cli::teams_config::ReviewKind::Commit,
                    },
                },
                picker: &picker,
                log_cursor: 0,
                lift: 0,
            },
        )
        .0
        .join("\n");
        assert!(add.contains("CONFIRM · AGENTS"), "full-screen title: {add}");
        assert!(
            add.contains("add reviewer") && add.contains("ruthless [claude]"),
            "names the add target: {add}"
        );
        assert!(
            add.contains("[Y]es") && add.contains("⏎ = yes"),
            "add default is Yes: {add}"
        );
        assert!(
            !add.contains("confirm:"),
            "old inline prompt removed: {add}"
        );
        assert!(
            !add.contains("+ add agent"),
            "confirm page replaces the normal agents panel: {add}"
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
        // The agents section leads the header with NO title rule
        // (tui-gauges-declutter): locate it by its final ROW.
        let (agents_rows, queue_rule, log_rule) = (
            pos("+ add agent").expect("agents rows"),
            pos("QUEUE").expect("queue rule"),
            pos("LOG").expect("log rule"),
        );
        assert!(
            agents_rows < queue_rule && queue_rule < log_rule,
            "QUEUE sits between the agent rows and LOG"
        );
        assert!(pos("AGENTS").is_none(), "no AGENTS title rule");
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
        let (agents_rows, stash_rule, queue_rule, log_rule) = (
            pos("+ add agent").expect("agents rows"),
            pos("STASH").expect("stash rule"),
            pos("QUEUE").expect("queue rule"),
            pos("LOG").expect("log rule"),
        );
        assert!(
            agents_rows < stash_rule && stash_rule < queue_rule && queue_rule < log_rule,
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
            plan_page: None,
            event_page: None,
            wait_page: None,
            plan_input: None,
            mode: Mode::LogScroll,
            picker: &[],
            log_cursor: 1,
            lift: 0,
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
    fn the_fetched_body_precedes_the_metadata() {
        // In a short pane the first rows are all the operator sees, so
        // the content must be there rather than below the facts and
        // every copy row (codex on dcc04f9).
        use super::super::event_content::{ContentSlot, ContentState, EventBody};
        use clank_core::wait::ContentRef;

        let ev = crate::cli::github_timeline::MergedEvent {
            at: 0,
            repo: "o/r".into(),
            event: "pr_comment".into(),
            detail: None,
            number: Some(12),
            title: Some("Add the widget".into()),
            actor: Some("hubot".into()),
            url: Some("https://github.com/o/r/pull/12".into()),
            seen_by: vec!["alpha".into()],
            unhandled: true,
            baseline: false,
            members: vec![crate::cli::github_timeline::MemberRef {
                key: crate::cli::github_timeline::MemberKey {
                    agent: "alpha".into(),
                    source: "github-o-r-aaaa".into(),
                    seq: 1,
                    ident: String::new(),
                },
                acked: false,
                transport: crate::cli::github_event_log::Transport::Poll,
                content: None,
            }],
        };
        let mut ep = super::super::input::EventPage {
            target: ev.members[0].key.clone(),
            retained: ev.members.iter().map(|m| m.key.clone()).collect(),
            event: ev,
            prompts: Vec::new(),
            scroll: 0,
            content: None,
        };
        let mut slot = ContentSlot::requesting(ContentRef::Pr { number: 1 }, 1);
        slot.state = ContentState::Ready(EventBody {
            body: "THE-BODY-TEXT".into(),
            author: None,
            url: None,
        });
        ep.content = Some(slot);

        let (lines, _) =
            render_event_detail(&ep, &super::super::input::actions_for(&ep), 0, 80, 60);
        let joined = lines.join("\n");
        // Compare against a FACT VALUE, not the word "event": the
        // action row's own copy says "launch the event URL", which a
        // looser search matches before the facts even start.
        let body_at = joined.find("THE-BODY-TEXT").expect("body rendered");
        let facts_at = joined.find("pr_comment").expect("facts rendered");
        let members_at = joined.find("alpha").expect("copy rows rendered");
        assert!(
            body_at < facts_at && body_at < members_at,
            "the fetched body must precede the facts ({facts_at}) and the \
             copy rows ({members_at}), got body at {body_at}:\n{joined}"
        );
    }

    #[test]
    fn event_page_renders_actions_members_and_attributed_prompts() {
        // tui-github-event-page: the page shows its actions (browser
        // omitted without a URL, ack omitted when handled), every
        // member with per-copy transport + state, and prompts with
        // attribution only when they differ.
        use crate::cli::github_event_log::Transport;
        use crate::cli::github_timeline::{MemberKey, MemberRef, MergedEvent};
        let member = |agent: &str, acked: bool, transport| MemberRef {
            key: MemberKey {
                agent: agent.into(),
                source: "github-o-r-aaaa".into(),
                seq: 3,
                ident: "f:1".into(),
            },
            acked,
            transport,
            content: None,
        };
        let ev = MergedEvent {
            at: 1,
            repo: "o/r".into(),
            event: "pr_comment".into(),
            detail: Some("review".into()),
            number: Some(12),
            title: Some("Add the widget".into()),
            actor: Some("hubot".into()),
            url: Some("https://github.com/o/r/pull/12".into()),
            seen_by: vec!["alpha".into(), "beta".into()],
            unhandled: true,
            baseline: false,
            members: vec![
                member("alpha", false, Transport::Poll),
                member("beta", true, Transport::Relay),
            ],
        };
        let ep = super::super::input::EventPage {
            target: ev.members[0].key.clone(),
            retained: ev.members.iter().map(|m| m.key.clone()).collect(),
            event: ev,
            prompts: vec![
                (Some("alpha".into()), "triage and reply".into()),
                (Some("beta".into()), "just ack it".into()),
            ],
            scroll: 0,
            content: None,
        };
        let actions = event_actions(true, true, false);
        let (out, _) = render_event_detail(&ep, &actions, 0, 40, 90);
        let text: String = out
            .iter()
            .map(|l| visible(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("GITHUB · O/R"), "{text}");
        assert!(
            text.contains("open in browser") && text.contains("ack"),
            "{text}"
        );
        assert!(
            text.contains("alpha  github-o-r-aaaa@3  poll  UNHANDLED"),
            "{text}"
        );
        assert!(
            text.contains("beta  github-o-r-aaaa@3  relay  handled"),
            "{text}"
        );
        assert!(text.contains("[alpha] triage and reply"), "{text}");
        assert!(text.contains("[beta] just ack it"), "{text}");

        // Age renders; the key summary is pinned on the last row.
        assert!(text.contains("age"), "{text}");
        assert!(
            visible(out.last().unwrap()).contains("esc/q back"),
            "key summary pinned: {:?}",
            out.last()
        );

        // Prompt text is control-char sanitized at the shared
        // events-show seam (a raw ESC inside operator text becomes a
        // space; newlines survive for the wrap).
        assert_eq!(
            crate::cli::events::sanitize_multiline("a\u{1b}[31mb\nc"),
            "a [31mb\nc"
        );
        // A LONG prompt exceeds a short pane and is REACHABLE via
        // the scroll window rather than clipped forever.
        let mut long = ep.clone();
        long.prompts = vec![(None, format!("marker-head {}", "x".repeat(600)))];
        let (out0, total) = render_event_detail(&long, &actions_full(), 0, 14, 40);
        assert!(total > 14, "long content exceeds the pane: {total}");
        let mut scrolled = long.clone();
        scrolled.scroll = total - 14;
        let (out1, _) = render_event_detail(&scrolled, &actions_full(), 0, 14, 40);
        assert_ne!(
            out0.iter().map(|l| visible(l)).collect::<Vec<_>>(),
            out1.iter().map(|l| visible(l)).collect::<Vec<_>>(),
            "scroll reaches later body lines"
        );
        let tail: String = out1.iter().map(|l| visible(l)).collect::<Vec<_>>().join("");
        assert!(tail.contains("xxx"), "the deep prompt lines are reachable");

        // Wide Unicode in a NARROW pane (codex bd110f0): every CJK
        // glyph survives the wrap — the display-width wrapper never
        // hands emit an over-wide chunk to truncate, so the full
        // sequence is recoverable through the scroll window.
        let cjk: String = "漢字寬度測試".repeat(40);
        let mut wide = ep.clone();
        wide.prompts = vec![(None, cjk.clone())];
        let (_, wide_total) = render_event_detail(&wide, &actions_full(), 0, 12, 24);
        let mut seen = String::new();
        for off in 0..wide_total {
            let mut page = wide.clone();
            page.scroll = off;
            let (out, _) = render_event_detail(&page, &actions_full(), 0, 12, 24);
            for l in &out {
                seen.push_str(visible(l).trim());
            }
        }
        for glyph in ["漢", "字", "寬", "度", "測", "試"] {
            assert!(
                seen.matches(glyph).count() >= 40,
                "every {glyph} reachable through scroll"
            );
        }

        // A one-row terminal renders EXACTLY one line (the footer) —
        // render's at-most-rows invariant (codex dd670c5).
        let (one, _) = render_event_detail(&ep, &actions_full(), 0, 1, 40);
        assert_eq!(one.len(), 1, "{one:?}");

        // No URL + fully handled: those actions are OMITTED.
        let actions = event_actions(false, false, false);
        assert_eq!(actions, vec![super::super::input::EventAction::Back]);
    }

    fn actions_full() -> Vec<super::super::input::EventAction> {
        event_actions(true, true, false)
    }

    #[test]
    fn adhoc_review_is_never_an_idle_bar() {
        // codex bad9de1: the panel row spun under a dim idle bar —
        // ad-hoc work must drive the SHARED attention/bar
        // derivation like every other in-flight kind. The bar names
        // WHO and the COMMIT (short sha), and attention classifies
        // Active (the zellij indicator follows it).
        let mut s = two_agent_snap(); // claude (master) + codex (commit)
        s.ad_hoc = vec![clank_core::wait::AdHocWorkState {
            sha: crate::lifecycle::CommitSha::parse(&format!("{:b<40}", "abc123")).unwrap(),
            gate: clank_core::vocab::CommitGateState::Unreviewed,
        }];
        assert_eq!(
            super::super::derive::attention_state(&s),
            super::super::derive::AttentionState::Active,
            "ad-hoc work is in-flight"
        );
        let (left, right) = bar_text(&s);
        assert!(left.contains("👀") && left.contains("CODEX"), "{left}");
        assert!(!left.contains("idle"), "{left}");
        assert_eq!(
            right,
            crate::cli::status::short_sha(s.ad_hoc[0].sha.as_str())
        );

        // ChangesRequested: the reviewers are done — master owes the
        // revision, and EVERY master surface agrees (codex 4a4be39):
        // the bar, the green frame (not promote-cyan, not
        // reviewer-yellow), the master 🔨, and the revising spinner
        // on master's row.
        s.ad_hoc[0].gate = clank_core::vocab::CommitGateState::ChangesRequested;
        let (left, _) = bar_text(&s);
        assert!(left.contains("🔨") && left.contains("CLAUDE"), "{left}");
        assert!(super::super::derive::master_is_active(&s));
        assert_eq!(super::super::derive::state_color(&s), Hue::Green, "master");
        assert_eq!(
            super::super::derive::agent_status_emoji(&s, "claude", clank_core::vocab::Role::Master),
            "🔨"
        );
        let out = render(&s, 40, 80);
        let claude_row = visible(line_with(&out, "claude"));
        assert!(
            claude_row.contains(SPINNER[0]) && claude_row.contains("revising"),
            "master row spins with the routed verb: {claude_row}"
        );

        // TERMINAL gates route no work (codex 923a679): a positively
        // reviewed ad-hoc commit is Idle — not a permanently active
        // bar falsely revising.
        for gate in [
            clank_core::vocab::CommitGateState::Continued,
            clank_core::vocab::CommitGateState::Finished,
        ] {
            s.ad_hoc[0].gate = gate;
            assert_eq!(
                super::super::derive::attention_state(&s),
                super::super::derive::AttentionState::Idle,
                "{gate:?} is terminal"
            );
            let (left, _) = bar_text(&s);
            assert!(left.contains("idle"), "{gate:?}: {left}");
        }

        // Head correction preempts an Unreviewed ad-hoc: correction
        // owns the state and the bar must NOT claim master is
        // revising.
        s.ad_hoc[0].gate = clank_core::vocab::CommitGateState::Unreviewed;
        s.head_correction = Some(clank_core::wait::HeadCorrection {
            sha: crate::lifecycle::CommitSha::parse(&"a".repeat(40)).unwrap(),
            violation: clank_core::wait::HeadTagViolation {
                unknown: vec!["ghost".to_string()],
                untagged_touched: vec![],
                extra_named: vec![],
            },
        });
        assert_eq!(
            super::super::derive::attention_state(&s),
            super::super::derive::AttentionState::NeedsCorrection
        );
        let (left, _) = bar_text(&s);
        assert!(!left.contains("revising"), "correction precedence: {left}");
    }

    #[test]
    fn adhoc_and_queue_mix_follows_waits_precedence() {
        // codex 0c31c92: precedence mirrors wait's routing. A
        // ChangesRequested ad-hoc is an actionable master item wait
        // returns BEFORE the queue scan — revising beats promote on
        // every surface. An Unreviewed ad-hoc gives master nothing,
        // so promotion wins the bar/frame while the reviewers still
        // spin their rows.
        let mut s = two_agent_snap(); // claude (master) + codex (commit)
        s.queue = vec![crate::cli::status::QueueItemView {
            priority: 500,
            name: "queued-plan".to_string(),
        }];
        s.ad_hoc = vec![clank_core::wait::AdHocWorkState {
            sha: crate::lifecycle::CommitSha::parse(&format!("{:b<40}", "abc123")).unwrap(),
            gate: clank_core::vocab::CommitGateState::ChangesRequested,
        }];
        let (left, right) = bar_text(&s);
        assert!(
            left.contains("🔨") && left.contains("revising"),
            "revising beats promote: {left}"
        );
        assert_eq!(
            right,
            crate::cli::status::short_sha(s.ad_hoc[0].sha.as_str())
        );
        assert_eq!(
            super::super::derive::state_color(&s),
            Hue::Green,
            "green, not cyan"
        );
        assert!(super::super::derive::master_is_active(&s));

        // Unreviewed + queue: promote wins the bar and the frame is
        // promote-cyan; master panes show active (promoting), the
        // reviewer still spins.
        s.ad_hoc[0].gate = clank_core::vocab::CommitGateState::Unreviewed;
        let (left, _) = bar_text(&s);
        assert!(left.contains("promote"), "promote wins the bar: {left}");
        assert_eq!(super::super::derive::state_color(&s), Hue::Cyan, "promote");
        assert!(super::super::derive::master_is_active(&s), "promoting");
        assert_eq!(
            super::super::derive::agent_status_emoji(
                &s,
                "codex",
                clank_core::vocab::Role::Reviewer
            ),
            "👀",
            "the reviewer is still awaited"
        );
        let out = render(&s, 40, 80);
        let codex_row = visible(line_with(&out, "codex"));
        assert!(
            codex_row.contains(SPINNER[0]) && codex_row.contains("reviewing"),
            "reviewer spins under a promote bar: {codex_row}"
        );
    }

    #[test]
    fn adhoc_review_spins_the_reviewer_row() {
        // tui-adhoc-review-activity: an ad-hoc commit review — no
        // plans at all — spins the reviewer's AGENTS row with the
        // "reviewing" verb; master's row stays still.
        let mut s = two_agent_snap(); // claude (master) + codex (commit)
        s.ad_hoc = vec![clank_core::wait::AdHocWorkState {
            sha: crate::lifecycle::CommitSha::parse(&"b".repeat(40)).unwrap(),
            gate: clank_core::vocab::CommitGateState::Unreviewed,
        }];
        let out = render(&s, 40, 80);
        let codex_row = visible(line_with(&out, "codex"));
        assert!(
            codex_row.contains(SPINNER[0]) && codex_row.contains("reviewing"),
            "ad-hoc review spins the reviewer row: {codex_row}"
        );
        let claude_row = visible(line_with(&out, "claude"));
        assert!(
            !claude_row.contains(SPINNER[0]),
            "master row still: {claude_row}"
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

    /// An attending agent is blocked, not working. The hourglass takes
    /// the spinner's place rather than joining it: an animated
    /// `working…` beside a wait marker asserts two states at once, and
    /// the moving one is the wrong one.
    #[test]
    fn an_attending_agent_row_shows_the_hourglass_instead_of_a_verb() {
        let mut s = two_agent_snap();
        s.plans = vec![plan_state("foo", WaitingOn::MasterToContinue)];
        s.agents[0].attending = Some(crate::cli::stop_hook::Attended {
            desc: None,
            token: None,
            task: "b72qah60w".to_string(),
            pid: Some(std::process::id() as i32),
            at: "2026-08-20T14:51:09Z".to_string(),
        });
        let lines = render(&s, 40, 80);
        let agent = visible(line_with(&lines, "claude"));
        let wait = visible(line_with(&lines, "⌛"));
        assert!(
            !wait.is_empty(),
            "the wait gets a line of its own: {lines:?}"
        );
        assert!(
            !agent.contains("working") && !agent.contains(SPINNER[0]),
            "verb and spinner are replaced, not joined: {agent}"
        );
    }

    /// A dead pid is the process saying nobody is waiting. Rendering
    /// `stale, ended` spent the row's scarcest resource to say that,
    /// so the marker goes entirely and the row reads as any other.
    #[test]
    fn an_ended_wait_renders_no_marker_and_keeps_the_record() {
        let mut s = two_agent_snap();
        s.agents[0].attending = Some(crate::cli::stop_hook::Attended {
            desc: None,
            token: None,
            task: "b72qah60w".to_string(),
            // Above every platform's pid_max, so it cannot be running.
            pid: Some(i32::MAX),
            at: "2026-08-20T14:51:09Z".to_string(),
        });
        let joined = render(&s, 40, 80).join("\n");
        assert!(
            !joined.contains("⌛"),
            "an ended wait is not drawn: {joined}"
        );
        assert!(!joined.contains("stale"), "and says nothing about it");
        assert!(
            s.agents[0].attending.is_some(),
            "rendering must not consume the record — the hook owns reaping"
        );
    }

    /// A wait that cannot be CHECKED is not drawn.
    ///
    /// This inverts what it used to assert. Drawing such a record is a
    /// liveness claim — `⌛ b72qah60w · 2m` and climbing — made from a
    /// record that can never support one, and because nothing can ever
    /// learn the work ended, the row ages upward forever. Lloyd
    /// reported exactly that against a real `attending: claude →
    /// bk3qnvo12 · 2m` whose task had long finished.
    ///
    /// "Cannot check" is not "ended", which is why the old rule looked
    /// reasonable. But it is not "running" either, and the row only
    /// has the vocabulary to say running.
    #[test]
    fn a_wait_that_cannot_be_checked_is_not_drawn() {
        let mut s = two_agent_snap();
        s.agents[0].attending = Some(crate::cli::stop_hook::Attended {
            desc: None,
            token: None,
            task: "b72qah60w".to_string(),
            pid: None,
            at: "2026-08-20T14:51:09Z".to_string(),
        });
        assert!(
            !render(&s, 40, 80).join("\n").contains("⌛"),
            "an uncheckable record must not be shown as an ongoing wait"
        );

        // The control: give it a pid that IS alive and the wait draws,
        // so the assertion above is about checkability and not about a
        // snapshot that never drew a marker at all.
        s.agents[0].attending = Some(crate::cli::stop_hook::Attended {
            desc: None,
            token: None,
            task: "b72qah60w".to_string(),
            pid: Some(std::process::id() as i32),
            at: "2026-08-20T14:51:09Z".to_string(),
        });
        let wait = visible(line_with(&render(&s, 40, 80), "⌛"));
        assert!(
            wait.contains("b72qah60w"),
            "a checkable one draws, named by its id when no description was recorded: {wait}"
        );
    }

    /// The subject is what the marker exists to show: the recorded
    /// description when there is one, the task id when there is not.
    /// Naming nothing was the bug — a row reading `⌛ 2m` announced a
    /// wait and withheld what it was for.
    #[test]
    fn the_marker_names_its_subject_description_first_then_the_id() {
        let att = |desc: Option<&str>| crate::cli::stop_hook::Attended {
            desc: desc.map(str::to_string),
            token: None,
            task: "b72qah60w".to_string(),
            pid: Some(std::process::id() as i32),
            at: "2026-08-20T14:51:09Z".to_string(),
        };

        let mut s = two_agent_snap();
        s.agents[0].attending = Some(att(Some("test run")));
        let wait = visible(line_with(&render(&s, 40, 120), "⌛"));
        assert!(
            wait.contains("test run"),
            "the description is the subject: {wait}"
        );
        assert!(
            !wait.contains("b72qah60w"),
            "the opaque id gives way to it: {wait}"
        );
        assert!(
            wait.contains(&std::process::id().to_string()),
            "the pid still rides along when there is room: {wait}"
        );

        s.agents[0].attending = Some(att(None));
        let wait = visible(line_with(&render(&s, 40, 120), "⌛"));
        assert!(
            wait.contains("b72qah60w"),
            "with no description the id is the subject: {wait}"
        );
    }

    /// The invariant, over every shape a record can take: a drawn
    /// marker always names what it attends. `⌛ 2m` — waiting, for two
    /// minutes, on nothing it would name — is the bug this plan exists
    /// to make unrepresentable.
    #[test]
    fn every_drawn_marker_names_its_subject() {
        let now = time::OffsetDateTime::now_utc();
        let live = std::process::id() as i32;
        // Every shape that is DRAWN, which now means every shape with
        // a live pid — an uncheckable record is not rendered at all.
        for (desc, pid, at) in [
            (Some("test run"), Some(live), "2026-08-20T14:51:09Z"),
            (None, Some(live), "2026-08-20T14:51:09Z"),
            // Unparseable timestamp: no age, so the marker is subject
            // (and pid) alone — still never subjectless.
            (None, Some(live), "not-a-timestamp"),
        ] {
            let att = crate::cli::stop_hook::Attended {
                desc: desc.map(str::to_string),
                token: None,
                task: "b72qah60w".to_string(),
                pid,
                at: at.to_string(),
            };
            let fields = att.marker_fields(now).expect("a live wait is drawn");
            let marker = fit_marker(&fields, 80).expect("80 columns is ample");
            assert!(
                marker.contains(desc.unwrap_or("b72qah60w")),
                "marker names nothing for desc={desc:?} pid={pid:?} at={at}: {marker}"
            );
        }
    }

    /// Fields drop from the RIGHT as the pane narrows — pid, then age
    /// — so a narrowing line only ever gets shorter and the subject is
    /// the last thing standing.
    #[test]
    fn the_marker_drops_pid_then_age_and_keeps_the_subject() {
        // Built directly: this exercises the FITTER, and routing it
        // through a record would make the test depend on whether a
        // fabricated pid happens to be alive.
        let f = crate::cli::stop_hook::MarkerFields {
            subject: "test run",
            age: Some("2m".to_string()),
            pid: Some(41293),
        };

        let wide = fit_marker(&f, 60).expect("fits");
        assert!(wide.contains("test run") && wide.contains("41293"));

        // Enough for subject + age, not the pid.
        let mid = fit_marker(&f, 18).expect("subject and age fit");
        assert!(mid.contains("test run"), "subject survives: {mid}");
        assert!(!mid.contains("41293"), "pid dropped first: {mid}");

        // Enough for the subject alone.
        let narrow = fit_marker(&f, 11).expect("subject fits");
        assert_eq!(narrow, "⌛ test run", "age dropped next");
    }

    /// Below the floor there is no marker at all — not a bare
    /// hourglass, and not an ellipsis naming nothing. `⌛ t…` is the
    /// reported bug wearing a different hat, so the width case is the
    /// invariant applied, never an exception to it.
    #[test]
    fn a_pane_too_narrow_to_name_the_wait_draws_no_marker() {
        let f = crate::cli::stop_hook::MarkerFields {
            subject: "running the whole test suite",
            age: Some("2m".to_string()),
            pid: Some(41293),
        };

        // Room for some of the subject: truncated, still identifying.
        let cut = fit_marker(&f, 14).expect("above the floor");
        assert!(
            cut.starts_with("⌛ running"),
            "keeps a readable head: {cut}"
        );
        assert!(cut.contains('…'), "and marks the cut: {cut}");

        // Under the floor: nothing at all, at every width down to zero.
        for budget in 0..=8 {
            assert_eq!(
                fit_marker(&f, budget),
                None,
                "a {budget}-column pane must draw NO marker, never a bare hourglass"
            );
        }
    }

    /// Being blocked and being displayable are different facts. An
    /// undrawable marker must not make the agent claim it is WORKING —
    /// a false statement, strictly worse than the silence the width
    /// rule asks for.
    ///
    /// The discriminating case is a live wait whose subject cannot be
    /// named at ANY width, not a narrow pane: the verb needs about 29
    /// columns and a marker only 12, so at every width narrow enough
    /// to lose the marker the verb is already truncated away and the
    /// assertion cannot tell the wirings apart. A persisted empty task
    /// separates them at full width.
    #[test]
    fn an_undrawable_wait_still_stops_the_agent_claiming_it_is_working() {
        let mut s = two_agent_snap();
        s.plans = vec![plan_state("foo", WaitingOn::MasterToContinue)];
        s.agents[0].attending = Some(crate::cli::stop_hook::Attended {
            desc: None,
            token: None,
            // Nothing to name: the record is live but unnameable.
            task: String::new(),
            pid: Some(std::process::id() as i32),
            at: "2026-08-20T14:51:09Z".to_string(),
        });
        let lines = render(&s, 40, 80);
        let agent = visible(line_with(&lines, "claude"));
        assert!(
            !lines.join("\n").contains("⌛"),
            "an unnameable wait draws no marker: {lines:?}"
        );
        assert!(
            !agent.contains("working") && !agent.contains(SPINNER[0]),
            "but the agent is still BLOCKED and must not spin: `{agent}`"
        );

        // The control: remove the attendance and the verb returns, so
        // the assertion above is about the wiring, not a snapshot that
        // never had a verb.
        s.agents[0].attending = None;
        let agent = visible(line_with(&render(&s, 40, 80), "claude"));
        assert!(
            agent.contains("working") && agent.contains(SPINNER[0]),
            "a genuinely unattended master does spin: `{agent}`"
        );
    }

    /// The narrow pane, at the full render call site: no marker line,
    /// and nothing left behind where it would have been.
    #[test]
    fn a_narrow_pane_draws_no_marker_line_at_all() {
        let mut s = two_agent_snap();
        s.agents[0].attending = Some(crate::cli::stop_hook::Attended {
            desc: Some("running the whole test suite".to_string()),
            token: None,
            task: "b72qah60w".to_string(),
            pid: Some(std::process::id() as i32),
            at: "2026-08-20T14:51:09Z".to_string(),
        });
        // 11 columns: two to the indent, three to the glyph, leaving
        // six for the subject — one short of the floor once the
        // ellipsis takes its column.
        let lines = render(&s, 40, 11);
        assert!(
            !lines.join("\n").contains("⌛"),
            "no marker fits at 11 columns: {lines:?}"
        );
        // And at 12 it does — so the assertion above is a real
        // boundary, not a width where nothing renders anyway.
        assert!(
            render(&s, 40, 12).join("\n").contains("⌛"),
            "12 columns is exactly enough"
        );
    }

    /// The subject is drawn into a terminal ROW. A description with a
    /// newline would inject a second one and break the height clamp;
    /// control bytes would emit escape sequences. And a record whose
    /// task was empty all along names nothing, so it draws nothing.
    #[test]
    fn a_subject_is_one_safe_line_or_there_is_no_marker() {
        let f = |subject: &'static str| crate::cli::stop_hook::MarkerFields {
            subject,
            age: Some("2m".to_string()),
            pid: None,
        };

        let m = fit_marker(&f("test\nrun"), 80).expect("first line survives");
        assert_eq!(m, "⌛ test · 2m", "a newline cannot add a row: {m}");

        let m = fit_marker(&f("test\u{1b}[31m run"), 80).expect("stripped");
        assert!(
            !m.contains('\u{1b}'),
            "no escape sequences reach the pane: {m}"
        );

        // Nothing legible left: no marker, never a bare hourglass.
        assert_eq!(
            fit_marker(&f(""), 80),
            None,
            "an empty subject draws nothing"
        );
        assert_eq!(fit_marker(&f("   "), 80), None, "nor a blank one");
        assert_eq!(
            fit_marker(&f("\u{1b}\u{7}"), 80),
            None,
            "nor one that is only control bytes"
        );
    }

    /// The wait sits on its own line, one step in from its agent — and
    /// the agents themselves no longer carry the indent they used to
    /// spend on nothing.
    #[test]
    fn the_wait_gets_an_indented_line_under_a_flush_agent_row() {
        let mut s = two_agent_snap();
        s.agents[0].attending = Some(crate::cli::stop_hook::Attended {
            desc: Some("test run".to_string()),
            token: None,
            task: "b72qah60w".to_string(),
            pid: Some(std::process::id() as i32),
            at: "2026-08-20T14:51:09Z".to_string(),
        });
        let lines = render(&s, 40, 80);
        let agent_at = lines
            .iter()
            .position(|l| visible(l).contains("claude"))
            .unwrap();
        let wait_at = lines
            .iter()
            .position(|l| visible(l).contains("⌛"))
            .unwrap();
        assert_eq!(wait_at, agent_at + 1, "the wait follows its agent");

        let agent = visible(&lines[agent_at]);
        let wait = visible(&lines[wait_at]);
        assert!(
            !agent.starts_with(' '),
            "the agent row is flush now: `{agent}`"
        );
        assert!(
            wait.starts_with(ATTENDING_INDENT) && !wait.trim_start().is_empty(),
            "the wait is indented under it: `{wait}`"
        );
    }

    /// The page says WHY it cannot stop a process, instead of just
    /// omitting the control. An absent button with no explanation
    /// teaches nothing, and each of these has a different remedy.
    #[test]
    fn the_wait_page_explains_every_reason_it_cannot_stop_a_process() {
        let att = |pid: Option<i32>, token: Option<crate::proc_identity::ProcToken>| {
            crate::cli::stop_hook::Attended {
                task: "b72qah60w".to_string(),
                desc: Some("test run".to_string()),
                pid,
                token,
                at: "2026-08-20T14:51:09Z".to_string(),
            }
        };
        let me = std::process::id() as i32;
        let body = |a: &crate::cli::stop_hook::Attended| {
            let actions = wait_actions(a.killable_pid().is_some());
            render_wait_page("claude", a, &actions, 0, 40, 80)
                .0
                .iter()
                .map(|l| visible(l))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let no_pid = body(&att(None, None));
        assert!(no_pid.contains("no pid"), "{no_pid}");
        assert!(!no_pid.contains("stop this process"), "no control offered");

        let no_token = body(&att(Some(me), None));
        assert!(
            no_token.contains("before clank identified processes"),
            "{no_token}"
        );

        let dead = body(&att(
            Some(i32::MAX),
            Some(crate::proc_identity::ProcToken::MacStart { sec: 1, usec: 1 }),
        ));
        assert!(dead.contains("already ended"), "{dead}");

        let reused = body(&att(
            Some(me),
            Some(crate::proc_identity::ProcToken::MacStart { sec: 1, usec: 1 }),
        ));
        assert!(reused.contains("reused"), "{reused}");
        assert!(!reused.contains("stop this process"), "and offers nothing");

        // The one case that CAN be stopped offers the control and
        // explains nothing.
        let live = body(&att(Some(me), crate::proc_identity::token_for(me)));
        assert!(live.contains("stop this process"), "{live}");
        assert!(!live.contains("cannot stop it"), "{live}");
    }

    /// The confirm names the blast radius rather than implying a clean
    /// tree kill: one pid is signalled, its children are not.
    #[test]
    fn the_kill_confirm_says_what_it_does_not_reach() {
        let me = std::process::id() as i32;
        let att = crate::cli::stop_hook::Attended {
            task: "b72qah60w".to_string(),
            desc: Some("test run".to_string()),
            pid: Some(me),
            token: crate::proc_identity::token_for(me),
            at: "2026-08-20T14:51:09Z".to_string(),
        };
        let body = render_kill_confirm("claude", &att, 40, 80)
            .0
            .iter()
            .map(|l| visible(l))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(body.contains(&me.to_string()), "names the target: {body}");
        assert!(
            body.contains("children"),
            "and what it will not reach: {body}"
        );
        assert!(
            body.contains("record is kept"),
            "and that nothing is reaped: {body}"
        );
    }

    /// The regression that took the layout down: `⌛` is U+231B, two
    /// columns, measured as one. Every attending row came out a column
    /// over, wrapped, and pushed the status bar off screen.
    ///
    /// Asserts the row is BUILT to fit: nothing over `cols`, the
    /// selection band included. Two things it deliberately does NOT
    /// assert:
    ///
    /// The band is not checked for being `cols` wide — agent-panel
    /// bands hug their own content (a selected `codex` row is 18
    /// columns in a 40-column pane), so demanding pane width here
    /// would pin a model the renderer does not implement.
    ///
    /// And it cannot catch `char_width` being wrong, since it measures
    /// with the same function the renderer used. That guard is
    /// `every_drawn_glyph_is_measured`, which pins against East-Asian
    /// width rather than against ourselves.
    #[test]
    fn an_attending_row_is_built_to_fit_its_pane() {
        let mut s = two_agent_snap();
        s.agents[0].attending = Some(crate::cli::stop_hook::Attended {
            desc: None,
            token: None,
            task: "b72qah60w".to_string(),
            pid: Some(std::process::id() as i32),
            at: "2026-08-20T14:51:09Z".to_string(),
        });
        for cols in [18u16, 24, 40, 80] {
            for line in &render(&s, 40, cols) {
                assert!(
                    display_width(visible(line).trim_end()) <= cols as usize,
                    "line exceeds {cols} display cols: `{line}`"
                );
            }

            let lines = render_at(
                &s,
                40,
                cols,
                0,
                0,
                &PanelView::just(Mode::AgentPanel { sel: 0 }),
            )
            .0;
            let row = line_with(&lines, "claude").to_string();
            assert!(
                row.contains(REVERSE),
                "the attending row is selected: {row}"
            );
            assert!(
                display_width(&visible(&row)) <= cols as usize,
                "the selection band overruns a {cols}-column pane: `{}`",
                visible(&row)
            );
        }
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

    fn doc<'a>(
        short_sha: &'a str,
        subject: &'a str,
        body: &'a str,
        stats: &'a [crate::git_io::FileStat],
        reviews: &'a [(String, clank_core::vocab::Verdict, String)],
    ) -> CommitDoc<'a> {
        CommitDoc {
            short_sha,
            subject,
            body,
            stats,
            reviews,
        }
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
            &doc(
                "abc1234",
                "do the thing",
                "a longer body paragraph",
                &[],
                &reviews,
            ),
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
            &doc(
                "abc1234",
                "do the thing",
                "a longer body paragraph",
                &[],
                &reviews,
            ),
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
    fn commit_detail_orders_hash_title_stats_body_reviews() {
        use clank_core::vocab::Verdict;
        let stats = vec![
            crate::git_io::FileStat {
                path: "src/lib.rs".into(),
                added: Some(10),
                removed: Some(2),
            },
            crate::git_io::FileStat {
                path: "assets/logo.png".into(),
                added: None,
                removed: None,
            },
        ];
        let reviews = vec![("codex".to_string(), Verdict::Continue, "lgtm".to_string())];
        let layout = build_commit_lines(
            &doc(
                "abc123456789",
                "do the thing",
                "the body prose",
                &stats,
                &reviews,
            ),
            80,
        );
        let vis: Vec<String> = layout.lines.iter().map(|l| visible(l)).collect();
        let pos = |needle: &str| {
            vis.iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("`{needle}` missing: {vis:#?}"))
        };
        // The shape of the change BEFORE the prose about it.
        let hash = pos("abc123456789");
        let title = pos("do the thing");
        let stat = pos("src/lib.rs");
        let summary = pos("2 files · +10 −2");
        let body = pos("the body prose");
        let review = pos("codex");
        assert!(hash < title, "hash above the title");
        assert!(title < stat && stat < summary, "stats after the title");
        assert!(summary < body, "stats BEFORE the message body");
        assert!(body < review, "reviews last");
        // Per-file counts + binary marker.
        assert!(vis[stat].contains("+10") && vis[stat].contains("−2"));
        assert!(vis[pos("assets/logo.png")].contains("bin"), "binary → bin");
        // Reviewer-jump offsets derive from the SAME layout, so they
        // land on the header row even with the stat block present.
        let off = commit_review_offset(
            &doc(
                "abc123456789",
                "do the thing",
                "the body prose",
                &stats,
                &reviews,
            ),
            "codex",
            80,
        );
        assert_eq!(off, layout.review_line["codex"]);
        assert!(visible(&layout.lines[off]).contains("codex"));
    }

    #[test]
    fn long_stat_paths_keep_the_filename() {
        let long = "crates/cli/src/cli/status_tui/some/very/deep/nested/module/render_helpers.rs";
        let t = middle_truncate_path(long, 30);
        assert!(t.chars().count() <= 30, "{t}");
        assert!(t.ends_with("render_helpers.rs"), "filename kept: {t}");
        assert!(t.contains('…'), "visibly truncated: {t}");
        assert_eq!(
            middle_truncate_path("short.rs", 30),
            "short.rs",
            "short paths untouched"
        );
    }

    #[test]
    fn commit_detail_wraps_a_long_subject_instead_of_truncating() {
        // A subject wider than the pane must WRAP across multiple lines with
        // every word readable — never truncated with an ellipsis.
        let subject =
            "[a-really-long-plan-name] make the thing work end to end without going off the screen";
        let cols = 40;
        let (lines, _) = render_commit_detail(&doc("abc1234", subject, "", &[], &[]), 0, 40, cols);
        let visibles: Vec<String> = lines.iter().map(|l| visible(l)).collect();
        let joined = visibles.join("\n");
        // Every word of the subject survives (nothing dropped to an
        // ellipsis) — the strongest anti-truncation check.
        for word in subject.split_whitespace() {
            assert!(joined.contains(word), "word `{word}` missing: {joined}");
        }
        // The hash sits on its OWN line ABOVE the title — the title is
        // never indented by a sha gutter (tui-commit-overlay-stats).
        let sha_line = visibles
            .iter()
            .position(|l| l.contains("abc1234"))
            .expect("sha line present");
        assert_eq!(
            visibles[sha_line].trim(),
            "abc1234",
            "hash line carries only the hash"
        );
        let title_line = visibles
            .iter()
            .position(|l| l.contains("[a-really-long-plan-name]"))
            .expect("title present");
        assert!(title_line > sha_line, "title below the hash");
        assert!(
            visibles[title_line].starts_with("[a-really-long-plan-name]"),
            "title flush-left, no gutter: {:?}",
            visibles[title_line]
        );
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
        let layout = build_commit_lines(
            &doc("abc1234", "subject", "the message body", &[], &reviews),
            cols,
        );
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
            &doc("abc1234", "subject", "the message body", &[], &reviews),
            "zzz",
            cols,
        );
        assert_eq!(z, layout.review_line["zzz"]);
        let ghost = commit_review_offset(
            &doc("abc1234", "subject", "the message body", &[], &reviews),
            "ghost",
            cols,
        );
        assert_eq!(ghost, *layout.review_line.values().min().unwrap());
        assert_eq!(
            commit_review_offset(&doc("abc1234", "s", "b", &[], &[]), "x", cols),
            0
        );
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

    /// The picker frames itself by what choosing will DO, so a swap
    /// cannot be mistaken for an add — the two differ only in framing
    /// and share one candidate list.
    #[test]
    fn the_swap_picker_names_who_is_going_out() {
        let picker = vec![
            cand("scout", "codex", "codex"),
            cand("ruthless", "claude", "claude"),
        ];
        let (lines, _) = render_candidate_screen(&picker, 24, 60, 0, Some("kimi"), None);
        let text = lines
            .iter()
            .map(|l| visible(l))
            .collect::<Vec<_>>()
            .join("\n");
        let lower = text.to_lowercase();
        assert!(
            lower.contains("swap out kimi"),
            "names the outgoing agent: {text}"
        );
        assert!(lower.contains("swap in"), "and what Enter does: {text}");
        assert!(
            text.contains("scout") && text.contains("ruthless"),
            "{text}"
        );

        // The add framing is unchanged.
        let (lines, _) = render_candidate_screen(
            &picker,
            24,
            60,
            0,
            None,
            Some(crate::cli::teams_config::ReviewKind::Commit),
        );
        let lower = lines
            .iter()
            .map(|l| visible(l))
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();
        assert!(lower.contains("add a reviewer"), "{lower}");
        assert!(!lower.contains("swap"), "{lower}");
    }

    /// Swap opens a picker rather than acting, so it must not read as
    /// a toggle or a destructive action.
    #[test]
    fn the_swap_action_row_reads_as_a_navigation() {
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;
        let agent = agent_row("kimi", RosterRole::Commit, AutoMode::On);
        let actions = detail_actions(RosterRole::Commit);
        let sel = actions
            .iter()
            .position(|a| *a == DetailAction::Swap)
            .expect("swap is on a reviewer page");
        let (lines, _) = render_agent_detail(&agent, &actions, sel, 24, 60);
        let row = lines
            .iter()
            .map(|l| visible(l))
            .find(|t| t.contains("swap"))
            .expect("swap row");
        assert!(
            row.contains("swap for another"),
            "names what it opens: {row:?}"
        );
        assert!(
            !row.contains('…'),
            "`…` means TRUNCATED in this UI; a label must not borrow it: {row:?}"
        );
        // Not red: remove owns the destructive colour.
        let raw = lines.join("\n");
        let red_swap = raw
            .lines()
            .any(|l| l.contains("swap") && l.contains("\x1b[31m"));
        assert!(!red_swap, "swap is not destructive styling");
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
