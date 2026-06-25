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

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use super::term::{AltScreen, paint, term_size};

use super::status::{StatusSnapshot, spawn_sigwinch_forwarder, watch_status_paths};

mod text;
use text::*;

mod derive;

mod input;
use input::*;

mod render;
use render::*;

mod scroll;
use scroll::*;

mod zellij;
use zellij::{PaneStatus, TabIndicator};

// ── terminal plumbing ───────────────────────────────────────
// The raw-mode alt-screen lifecycle, the size probe, and the frame
// painter live in `super::term` (shared with the console).

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

/// The log viewport's scroll state, bundled so the invariants that used
/// to live in loose-variable comments are enforced by methods:
/// - the `offset` (viewport top) is always DERIVED from the `cursor` via
///   [`settle`](LogView::settle) — never set independently while the log
///   is focused (the one exception, crossing in from the panel, goes
///   through [`enter_first`](LogView::enter_first), which resets both);
/// - `fill` (permission to do the top-up IO) is REQUESTED by input/data/
///   resize events and consumed once per fill — an animation tick never
///   requests it, so a tick stays a pure repaint.
///
/// The cursor moves are methods so the loop reads as intent
/// (`log.page_down(page)`) and the saturation lives in one place.
struct LogView {
    /// Selected entry — index into the scroll sequence.
    cursor: usize,
    /// Viewport top, derived from `cursor`.
    offset: usize,
    /// Log fetch-window size; grows on demand until `complete`.
    window: usize,
    /// The fetch reached the root — stop growing.
    complete: bool,
    /// This pass may do the top-up IO. Set by input/data/resize, cleared
    /// after one fill; a tick never sets it.
    fill: bool,
}

impl LogView {
    fn new() -> Self {
        Self {
            cursor: 0,
            offset: 0,
            window: 30,
            complete: false,
            fill: true,
        }
    }

    /// Permit a top-up fill on this pass (input, data change, resize).
    fn request_fill(&mut self) {
        self.fill = true;
    }

    /// Settle the viewport for the paint: clamp the cursor into the
    /// loaded length and DERIVE the offset from it when the log is
    /// focused; otherwise just keep the last page full.
    fn settle(&mut self, capacity: usize, total: usize, focused: bool) {
        if focused {
            self.cursor = self.cursor.min(total.saturating_sub(1));
            self.offset = scroll_to_show(self.cursor, self.offset, capacity, total);
        } else {
            self.offset = self.offset.min(total.saturating_sub(capacity.max(1)));
        }
    }

    fn up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
    fn down(&mut self) {
        self.cursor += 1;
    }
    fn page_up(&mut self, page: usize) {
        self.cursor = self.cursor.saturating_sub(page);
    }
    fn page_down(&mut self, page: usize) {
        self.cursor += page;
    }
    fn jump_top(&mut self) {
        self.cursor = 0;
    }
    fn jump_bottom(&mut self, total: usize) {
        self.cursor = total.saturating_sub(1);
    }
    /// Enter the log at its first entry (crossing in from the panel) —
    /// the one place offset is reset directly, alongside the cursor.
    fn enter_first(&mut self) {
        self.cursor = 0;
        self.offset = 0;
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

    // When inside zellij: mirror the bar's lamp emoji into the tab name,
    // and each agent's status glyph onto its own pane name.
    let mut tab = TabIndicator::new();
    let mut panes = PaneStatus::new();
    // Probe the input signature BEFORE the first build so any change
    // racing the build re-builds next wake (under-gate, never over-gate).
    let mut last_sig = crate::cli::status::input_signature(&repo).ok();
    let mut snapshot =
        StatusSnapshot::build_async(&repo, &basename, home.as_deref(), policy, None, true).await?;
    // The log viewport — cursor, derived offset, and the on-demand fetch
    // window. The log grows via fresh, larger windowed rebuilds (the fold
    // replays from a base, so this is re-fetch-bigger, not incremental):
    // keep at least `offset + viewport` rows loaded, which both fills a
    // tall pane on first paint and pages in older rows as you scroll. See
    // [`LogView`] for the invariants its methods enforce.
    let mut log = LogView::new();
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
    loop {
        let (rows, cols) = term_size();
        let log_focused = matches!(mode, Mode::LogScroll);
        let view = PanelView {
            mode,
            picker: &picker,
            log_cursor: log.cursor,
        };
        let capacity = render_at(&snapshot, rows, cols, log.offset, frame, &view).1;

        // The ask + in-progress rows depend on blocks/waiting_on (not the
        // log fetch), so compute them before filling. `head` is the count
        // of scrollable rows that aren't log rows (ask lines + in-progress
        // placeholders); the total scrollable length is head + log.
        let ask_lines = block_ask_spans(&snapshot, cols as usize);
        let in_prog = in_progress_rows(&snapshot);
        let head = ask_lines.len() + in_prog.len();

        // Load enough log to fill the viewport AND reach the cursor (the
        // cursor can move past the loaded tail). Gated on `log.fill` so
        // an animation tick never reaches it.
        if log.fill {
            let want = (log.offset + capacity).max(log.cursor + 1);
            while !log.complete && head + snapshot.log_rows.len() < want {
                log.window += capacity.max(1);
                let before = snapshot.log_rows.len();
                snapshot.log_rows = crate::cli::status::tui_log_rows(&repo, log.window).await;
                if snapshot.log_rows.len() == before {
                    log.complete = true; // hit the root — stop growing
                }
            }
            log.fill = false;
        }

        let total = head + snapshot.log_rows.len();
        // Derive the viewport from the cursor (focused) or keep the last
        // page full (unfocused) — see [`LogView::settle`].
        log.settle(capacity, total, log_focused);
        // Rebuild the view with the clamped cursor/offset for the paint.
        let view = PanelView {
            mode,
            picker: &picker,
            log_cursor: log.cursor,
        };
        paint(&render_at(&snapshot, rows, cols, log.offset, frame, &view).0);

        // The in-progress rows now sit at SCATTERED indices (master after
        // the plan header; reviews in the latest commit's review block), so
        // ask `build_scroll` (the same arrangement render uses) for their
        // positions and tick iff ANY of them is within the window.
        let win = log.offset..log.offset + capacity;
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
                                mode = mode.toggle_focus(snapshot.agents.len())
                            }
                            PanelAction::MoveCursor(s) => mode = Mode::AgentPanel { sel: s },
                            PanelAction::EnterLog => {
                                // Cross into the log at the FIRST entry.
                                log.enter_first();
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
                    // Default: the log is a selectable timeline — keys move
                    // the CURSOR entry (the viewport follows via
                    // scroll_to_show at the loop top). `Up` on the first
                    // entry crosses back into the panel (continuous nav).
                    Mode::LogScroll => match k {
                        Key::Quit => break,
                        Key::Focus => mode = mode.toggle_focus(snapshot.agents.len()),
                        Key::Up => match log_up_target(log.cursor, snapshot.agents.len()) {
                            Some(sel) => mode = Mode::AgentPanel { sel },
                            None => log.up(),
                        },
                        Key::Down => log.down(),
                        Key::PageUp => log.page_up(page),
                        Key::Space | Key::PageDown => log.page_down(page),
                        Key::Top => log.jump_top(),
                        Key::Bottom => log.jump_bottom(total),
                        Key::Escape | Key::Enter | Key::Delete | Key::Yes | Key::No => {}
                    },
                }
                log.request_fill();
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
                    snapshot.log_rows = crate::cli::status::tui_log_rows(&repo, log.window).await;
                    last_sig = sig;
                    log.complete = false;
                    log.request_fill();
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
                log.request_fill();
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
    use super::derive::*;
    use super::*;
    use crate::lifecycle::{AgentLabel, CommitSha, PlanKey};
    use clank_core::plan_view::WaitingOn;
    use clank_core::repo_state::NonEmptyVec;
    use clank_core::vocab::CommitGateState;
    use clank_core::wait::PlanWorkState;

    #[test]
    fn log_view_cursor_moves_saturate() {
        let mut v = LogView::new();
        assert_eq!((v.cursor, v.offset), (0, 0));
        v.up(); // at top → stays
        assert_eq!(v.cursor, 0);
        v.page_up(5); // at top → stays
        assert_eq!(v.cursor, 0);
        v.down();
        v.down();
        assert_eq!(v.cursor, 2);
        v.page_down(10);
        assert_eq!(v.cursor, 12);
        v.jump_top();
        assert_eq!(v.cursor, 0);
        v.jump_bottom(20); // last index of 20 entries
        assert_eq!(v.cursor, 19);
        v.jump_bottom(0); // empty timeline pins at 0
        assert_eq!(v.cursor, 0);
    }

    #[test]
    fn log_view_settle_derives_offset_and_enter_first_resets() {
        let mut v = LogView::new();
        // Focused: cursor below the window pulls the offset down to reveal
        // it (matches scroll_to_show), and a cursor past the end clamps.
        v.cursor = 50;
        v.settle(10, 20, true);
        assert_eq!(v.cursor, 19, "cursor clamped into the loaded length");
        assert_eq!(v.offset, scroll_to_show(19, 0, 10, 20));
        // Unfocused: the cursor is NOT clamped; only the offset is held to
        // the last page.
        let mut u = LogView::new();
        u.cursor = 50;
        u.offset = 999;
        u.settle(10, 20, false);
        assert_eq!(u.cursor, 50, "unfocused leaves the cursor alone");
        assert_eq!(u.offset, 10, "offset held to the last full page");
        // enter_first resets both (the one place offset is set directly).
        v.enter_first();
        assert_eq!((v.cursor, v.offset), (0, 0));
    }

    #[test]
    fn log_view_fill_is_requestable_and_one_shot() {
        // A fresh view wants its first fill; consuming it (as the loop's
        // fill block does) clears it, and request_fill re-arms it.
        let mut v = LogView::new();
        assert!(v.fill, "the first pass fills");
        v.fill = false; // loop consumes it
        assert!(!v.fill);
        v.request_fill();
        assert!(v.fill, "input/data/resize re-arm the fill");
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
        // With the CURSOR on the first entry, Up crosses back to the
        // panel's +add row (index == agents.len()); otherwise it moves
        // the cursor up (None).
        assert_eq!(log_up_target(0, 2), Some(2), "cursor 0 → +add row");
        assert_eq!(log_up_target(3, 2), None, "mid-log → move cursor up");
        assert_eq!(log_up_target(0, 0), None, "no roster → nothing to cross to");
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

    fn commit_row(subject: &str) -> crate::cli::log::OnelineRow {
        crate::cli::log::OnelineRow::Commit {
            sha: crate::lifecycle::CommitSha::parse(&format!("{:0<40}", "abc1234")).unwrap(),
            subject: subject.to_string(),
        }
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
    fn strip_leading_emoji_removes_a_stale_glyph_only() {
        // A prior, un-restored indicator must not stack.
        assert_eq!(strip_leading_emoji("👀 frostsnap"), "frostsnap");
        assert_eq!(strip_leading_emoji("💤 clank"), "clank");
        // A plain name is untouched.
        assert_eq!(strip_leading_emoji("clank"), "clank");
        // A name that merely starts with a word (no emoji) is untouched.
        assert_eq!(strip_leading_emoji("pr-497"), "pr-497");
    }

    pub(crate) fn pr_awaiting(missing: &[&str]) -> clank_core::wait::PrReviewWorkState {
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
    use clank_core::plan_view::WaitingOn;

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
