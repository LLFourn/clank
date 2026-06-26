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
// Re-export for `open_zellij`'s tab/pane renamer, which strips the same
// stale lamp prefix. (The IO shell itself uses no text primitives — the
// view modules do.)
pub(crate) use text::strip_leading_emoji;

mod derive;

mod markdown;

mod input;
use input::*;

mod render;
use render::*;

mod scroll;
use scroll::*;

mod zellij;
use zellij::{PaneStatus, TabIndicator};

#[cfg(test)]
pub(crate) mod fixtures;

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

// `strip_leading_emoji` lives in `text` (it's a name/width helper); the
// `--tui` loop's doc moved down onto `run_tui` where it belongs.

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

/// The fetched DATA for a commit-detail overlay: the commit and every
/// reviewer's verdict + full feedback body. Anchored by `sha` (not a log
/// index) so a `Refresh` re-fetches the SAME commit.
struct CommitDetail {
    sha: crate::lifecycle::CommitSha,
    short: String,
    subject: String,
    body: String,
    /// `(author, verdict, full feedback body)` per reviewer.
    reviews: Vec<(String, clank_core::vocab::Verdict, String)>,
}

/// What a full-window document overlay is showing. Each kind carries the
/// identity needed to re-fetch on a background `Refresh` (a commit sha; a
/// plan stem), so the watcher firing never loses the reader's place.
enum OverlayData {
    Commit(CommitDetail),
    /// A plan's markdown by stem; `markdown` is `None` when the file
    /// isn't found.
    Plan {
        stem: String,
        markdown: Option<String>,
    },
}

/// An open document overlay: the fetched [`OverlayData`] and the scroll
/// `offset`, kept SEPARATE so a background `Refresh` (which re-fetches the
/// data, so a new verdict / edited plan shows up live) swaps only the data
/// and never resets the reader's scroll position — the watcher fires
/// constantly in an active session. `Some` ⟺ an overlay is showing; the
/// log keeps its cursor underneath, restored on dismiss. Mirrors how
/// [`LogView`] keeps its cursor across refreshes.
struct Overlay {
    data: OverlayData,
    offset: usize,
}

impl Overlay {
    /// A commit overlay opened at `offset` (0 for a commit row; a
    /// reviewer's block for a review row — see [`render::commit_review_offset`]).
    fn commit(data: CommitDetail, offset: usize) -> Self {
        Self {
            data: OverlayData::Commit(data),
            offset,
        }
    }
    /// A plan-document overlay, opened at the top.
    fn plan(stem: String, markdown: Option<String>) -> Self {
        Self {
            data: OverlayData::Plan { stem, markdown },
            offset: 0,
        }
    }
    /// Swap in freshly-fetched data, PRESERVING the scroll offset (the
    /// next render's clamp handles content that shrank).
    fn refresh(&mut self, data: OverlayData) {
        self.data = data;
    }
    /// Scroll by a signed line delta, clamped to `[0, max_off]`.
    fn scroll(&mut self, delta: i32, max_off: usize) {
        self.offset = (self.offset as i32 + delta).clamp(0, max_off as i32) as usize;
    }
}

/// A reviewer's feedback as shown in the detail view: the full message
/// MINUS the leading verdict word (the verdict is already drawn as a
/// mark, so repeating "CONTINUE"/"REQUEST_CHANGES" would be noise). That
/// is the summary line + the details body — parsed via `FeedbackBody`,
/// the SAME path the log's review rows use, so the two never diverge.
/// (`details()` alone would drop a summary-only review's only content.)
fn feedback_display_body(raw: &str) -> String {
    let fb = clank_core::feedback_body::FeedbackBody::parse(raw);
    let summary = fb.summary();
    let details = fb.details();
    match (summary.is_empty(), details.is_empty()) {
        (false, false) => format!("{summary}\n\n{details}"),
        (false, true) => summary,
        (true, false) => details,
        (true, true) => String::new(),
    }
}

/// Fetch the commit subject/body (gix) and every reviewer's verdict +
/// full feedback body for `sha`. `FeedbackView` carries only the verdict
/// and a repo-relative `source_path` per entry, so the body is read from
/// that file (the same per-agent file the log's review summaries come
/// from). `None` if the commit can't be read; a missing feedback file
/// degrades to an empty body.
fn fetch_commit_detail(
    repo: &std::path::Path,
    sha: &crate::lifecycle::CommitSha,
) -> Option<CommitDetail> {
    let subject = crate::git_io::commit_subject_at(repo, sha).ok()?;
    let body = crate::git_io::commit_body_at(repo, sha).ok()?;
    let reviews = crate::feedback_scan::scan_feedback(repo, std::slice::from_ref(sha))
        .ok()
        .and_then(|view| {
            view.per_commit
                .into_iter()
                .find(|c| &c.sha == sha)
                .map(|c| {
                    c.entries
                        .into_iter()
                        .map(|(label, entry)| {
                            let raw = std::fs::read_to_string(repo.join(&entry.source_path))
                                .unwrap_or_default();
                            (
                                label.as_str().to_string(),
                                entry.verdict,
                                feedback_display_body(&raw),
                            )
                        })
                        .collect()
                })
        })
        .unwrap_or_default();
    Some(CommitDetail {
        short: crate::cli::status::short_sha(sha.as_str()).to_string(),
        sha: sha.clone(),
        subject,
        body,
        reviews,
    })
}

/// Read a plan stem's markdown from the WORKING TREE: the active
/// `.clank/plans/<stem>.md`, else the finalized `.clank/finished/<stem>.md`,
/// else `None`. A plain file read (like the feedback bodies above) — the
/// live file, not a git blob — so an in-progress edit shows immediately.
fn read_plan_markdown(repo: &std::path::Path, stem: &str) -> Option<String> {
    for rel in [
        crate::init_facts::plan_md_rel(stem),
        crate::init_facts::finished_md_rel(stem),
    ] {
        if let Ok(s) = std::fs::read_to_string(repo.join(&rel)) {
            return Some(s);
        }
    }
    None
}

/// Minimum wall-clock between status rebuilds. Coalesces a burst of
/// watcher wakes into at most one rebuild per interval — defense-in-depth
/// atop the (nested-aware) ignore filter, so even non-ignored churn can't
/// drive the rebuild loop hot.
const REBUILD_MIN: Duration = Duration::from_secs(1);

/// The loop's recv timeout: the `base` cadence (spinner tick when one is
/// visible, else the idle backstop), shortened to the time left before a
/// DEFERRED refresh may run — so a coalesced burst still rebuilds within
/// [`REBUILD_MIN`] (trailing edge), and a lone deferred wake fires within
/// the interval rather than waiting for the next unrelated event. Pure →
/// unit-tested.
fn deferred_wait(
    base: Duration,
    refresh_pending: bool,
    since_last_rebuild: Duration,
    min_interval: Duration,
) -> Duration {
    if refresh_pending {
        base.min(min_interval.saturating_sub(since_last_rebuild))
    } else {
        base
    }
}

/// A drained burst of loop events collapsed for ONE pass: keystrokes in
/// arrival order, and ANY number of `Refresh`/`Resize` folded to a single
/// flag each. So a storm of watcher wakes costs one repaint + at most one
/// (throttled) rebuild, not one per event.
struct Batch {
    keys: Vec<Key>,
    refresh: bool,
    resize: bool,
}

/// Fold a drained event burst into a [`Batch`]. Pure → unit-tested (the
/// "N wakes → one refresh" coalescing guarantee).
fn coalesce(events: impl IntoIterator<Item = Ev>) -> Batch {
    let mut b = Batch {
        keys: Vec::new(),
        refresh: false,
        resize: false,
    };
    for ev in events {
        match ev {
            Ev::Key(k) => b.keys.push(k),
            Ev::Refresh => b.refresh = true,
            Ev::Resize => b.resize = true,
        }
    }
    b
}

/// The `--tui` loop, fully event-driven: the watcher covers the
/// working tree (gitignore-filtered), `.clank/`, and the git dir;
/// SIGWINCH arrives on the same channel, so a resize is just
/// another wake. Between events there is nothing to redraw —
/// nothing rendered is clock-relative — so the only timeout is a
/// slow backstop against watcher pathologies the error channel
/// doesn't surface (tui-event-driven-dirty-stats).
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
    // The document overlay, when open (Enter on a log entry): a commit's
    // detail or a plan's rendered markdown.
    let mut detail: Option<Overlay> = None;
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
    // Debounce: a watcher wake only FLAGS a refresh; the trailing-edge
    // flush below coalesces a burst into ≤1 rebuild per REBUILD_MIN.
    // `last_rebuild` starts a full interval in the past so the first wake
    // rebuilds immediately (leading edge); later bursts coalesce.
    let mut last_rebuild = std::time::Instant::now()
        .checked_sub(REBUILD_MIN)
        .unwrap_or_else(std::time::Instant::now);
    let mut refresh_pending = false;
    // Labeled so a `Quit` key inside the per-key drain loop exits the
    // event loop, not just the inner `for`.
    'evloop: loop {
        let (rows, cols) = term_size();

        // The commit-detail overlay owns the whole pane until dismissed:
        // render it, route its own (scroll / back) keys, and skip the
        // normal log/panel path. Nothing here animates, so the wait is
        // the slow backstop.
        if detail.is_some() {
            let page = (rows as usize).saturating_sub(3).max(1);
            let overlay = detail.as_ref().unwrap();
            let (lines, total) = match &overlay.data {
                OverlayData::Commit(d) => render_commit_detail(
                    &d.short,
                    &d.subject,
                    &d.body,
                    &d.reviews,
                    overlay.offset,
                    rows as usize,
                    cols as usize,
                ),
                OverlayData::Plan { stem, markdown } => render_plan_doc(
                    stem,
                    markdown.as_deref(),
                    overlay.offset,
                    rows as usize,
                    cols as usize,
                ),
            };
            paint(&lines);
            match ev_rx.recv_timeout(Duration::from_secs(60)) {
                Ok(Ev::Key(k)) => match doc_nav(k, page) {
                    DocNav::Back => detail = None,
                    DocNav::Scroll(delta) => {
                        let max_off = total.saturating_sub((rows as usize).max(1));
                        detail.as_mut().unwrap().scroll(delta, max_off);
                    }
                    DocNav::None => {}
                },
                // Data changed under us — re-fetch by IDENTITY (commit sha /
                // plan stem), but KEEP the scroll offset (refresh swaps
                // data, not view-state).
                Ok(Ev::Refresh) => {
                    let new = match &detail.as_ref().unwrap().data {
                        OverlayData::Commit(d) => {
                            let sha = d.sha.clone();
                            fetch_commit_detail(&repo, &sha).map(OverlayData::Commit)
                        }
                        OverlayData::Plan { stem, .. } => {
                            let stem = stem.clone();
                            Some(OverlayData::Plan {
                                markdown: read_plan_markdown(&repo, &stem),
                                stem,
                            })
                        }
                    };
                    if let (Some(data), Some(o)) = (new, detail.as_mut()) {
                        o.refresh(data);
                    }
                }
                Ok(Ev::Resize) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("event channel disconnected")
                }
            }
            continue;
        }

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
        let base = if spinner_visible {
            Duration::from_millis(120)
        } else {
            Duration::from_secs(60)
        };
        let wait = deferred_wait(base, refresh_pending, last_rebuild.elapsed(), REBUILD_MIN);

        match ev_rx.recv_timeout(wait) {
            // Drain the whole queued burst in ONE pass: keystrokes apply
            // in arrival order, and any number of Refresh/Resize fold to a
            // single flag (coalesce), so a watcher storm costs one repaint
            // — not one loop turn per event.
            Ok(first) => {
                let batch = coalesce(
                    std::iter::once(first).chain(std::iter::from_fn(|| ev_rx.try_recv().ok())),
                );
                for k in batch.keys {
                    let page = (rows as usize).saturating_sub(3).max(1);
                    // `mode` is Copy: matching it copies, so reassigning `mode`
                    // inside an arm is free of borrow conflicts. The per-mode
                    // routing is PURE (agent_panel_action / agent_detail_nav /
                    // confirm_decision); the loop only executes the result
                    // (where IO happens).
                    match mode {
                        Mode::AgentPanel { sel } => {
                            match agent_panel_action(sel, &snapshot.agents, k) {
                                PanelAction::Quit => break 'evloop,
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
                                            if crate::agent_store::set_auto_mode(
                                                &repo, &label, next,
                                            )
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
                                DetailNav::Quit => break 'evloop,
                                DetailNav::Back => mode = Mode::AgentPanel { sel: idx },
                                DetailNav::MoveCursor(s) => {
                                    mode = Mode::AgentDetail { idx, sel: s }
                                }
                                DetailNav::Activate(action) => {
                                    mode =
                                        apply_detail_action(action, idx, sel, &mut snapshot, &repo);
                                }
                                DetailNav::None => {}
                            }
                        }
                        // Picker: choose a candidate to add.
                        Mode::AddPicker { sel } => match k {
                            Key::Quit => break 'evloop,
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
                            | Key::Left
                            | Key::Yes
                            | Key::No => {}
                        },
                        // Confirm: one decision, resolved in one place.
                        Mode::Confirm { action } => {
                            if let Some(go) = confirm_decision(action, k) {
                                if go {
                                    apply_confirm(
                                        action,
                                        &repo,
                                        home.as_deref(),
                                        &snapshot,
                                        &picker,
                                    );
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
                            Key::Quit => break 'evloop,
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
                            // Enter drills into the entry under the cursor:
                            // build the same scroll sequence the cursor indexes,
                            // map the entry to its document, fetch + open the
                            // overlay. A commit/review opens the commit detail (a
                            // review scrolled to that reviewer's feedback); a plan
                            // header opens the plan's rendered markdown. Ask /
                            // in-progress / ad-hoc rows resolve to None.
                            Key::Enter => {
                                let seq = build_scroll(&snapshot, &ask_lines, &in_prog);
                                match entry_overlay_target(&seq, log.cursor) {
                                    Some(OverlayTarget::Commit { sha, focus }) => {
                                        if let Some(data) = fetch_commit_detail(&repo, &sha) {
                                            let offset = match &focus {
                                                Some(author) => commit_review_offset(
                                                    &data.short,
                                                    &data.subject,
                                                    &data.body,
                                                    &data.reviews,
                                                    author,
                                                    cols as usize,
                                                ),
                                                None => 0,
                                            };
                                            detail = Some(Overlay::commit(data, offset));
                                        }
                                    }
                                    Some(OverlayTarget::Plan { stem }) => {
                                        let md = read_plan_markdown(&repo, &stem);
                                        detail = Some(Overlay::plan(stem, md));
                                    }
                                    None => {}
                                }
                            }
                            Key::Escape | Key::Left | Key::Delete | Key::Yes | Key::No => {}
                        },
                    }
                    log.request_fill();
                }
                // A data-change wake is COALESCED + THROTTLED: flag it; the
                // trailing-edge flush after the match rebuilds at most once
                // per REBUILD_MIN, so a churny tree can't drive the rebuild
                // loop hot. Resize tops up the (possibly taller) viewport.
                if batch.refresh {
                    refresh_pending = true;
                }
                if batch.resize {
                    log.request_fill();
                }
            }
            // Animation tick: advance the frame ONLY. `fill` stays false,
            // so the next iteration is a PURE repaint — render_at (pure) +
            // paint — with zero disk/git/zellij work.
            Err(mpsc::RecvTimeoutError::Timeout) if spinner_visible => {
                frame = frame.wrapping_add(1);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("event channel disconnected")
            }
        }

        // Trailing-edge refresh flush: the COALESCED snapshot rebuild, run
        // at most once per REBUILD_MIN (the only throttled work; keys and
        // resize above stay responsive). `deferred_wait` shortens the recv
        // timeout so a pending flush fires within the interval.
        if refresh_pending && last_rebuild.elapsed() >= REBUILD_MIN {
            refresh_pending = false;
            last_rebuild = std::time::Instant::now();
            // Nothing-changed gate (lloyd's invariant): a wake that touched
            // no snapshot input is dropped — no rebuild, no log re-fold, no
            // repaint. The probe is far cheaper than the work it guards.
            let sig = crate::cli::status::input_signature(&repo).ok();
            if sig != last_sig {
                // Capture the detail page's target by LABEL from the OLD
                // roster before rebuilding — an external promote / tier
                // change can REORDER rows (master, then commit, then gate),
                // so a kept index could silently retarget a different agent.
                // We re-locate the same label below.
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
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::fixtures::*;
    use super::*;

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

    #[test]
    fn deferred_wait_caps_to_the_refresh_interval() {
        let min = Duration::from_secs(1);
        let base = Duration::from_secs(60);
        // No pending refresh → the base cadence, untouched.
        assert_eq!(
            deferred_wait(base, false, Duration::from_millis(10), min),
            base
        );
        // Pending, interval not yet elapsed → wait the REMAINING time, so
        // the coalesced burst flushes within one interval (trailing edge).
        assert_eq!(
            deferred_wait(base, true, Duration::from_millis(400), min),
            Duration::from_millis(600)
        );
        // Pending, interval already elapsed → 0 (flush on the next tick).
        assert_eq!(
            deferred_wait(base, true, Duration::from_secs(5), min),
            Duration::ZERO
        );
        // Pending but a shorter base (a visible spinner) wins, so the
        // spinner keeps ticking while the refresh waits its interval.
        let spinner = Duration::from_millis(120);
        assert_eq!(
            deferred_wait(spinner, true, Duration::from_millis(100), min),
            spinner
        );
    }

    #[test]
    fn coalesce_collapses_a_burst_to_one_refresh() {
        // A storm of N Refresh (plus a key and a resize) → exactly ONE
        // refresh flag (so at most one throttled rebuild), the key kept in
        // order, resize folded — ONE pass, not N loop turns.
        let b = coalesce(vec![
            Ev::Refresh,
            Ev::Key(Key::Down),
            Ev::Refresh,
            Ev::Resize,
            Ev::Refresh,
        ]);
        assert!(b.refresh, "many Refresh fold to one flag");
        assert!(b.resize, "Resize folded");
        assert_eq!(b.keys, vec![Key::Down], "keys preserved in order");
        // 100 wakes still yield a single refresh flag and no spurious keys.
        let many = coalesce((0..100).map(|_| Ev::Refresh));
        assert!(many.refresh);
        assert!(many.keys.is_empty());
        assert!(!many.resize);
    }

    #[test]
    fn feedback_display_body_drops_verdict_word_keeps_content() {
        // The verdict is drawn as a mark, so the body must NOT repeat the
        // verdict header — but it must keep the reviewer's full message.
        let body = feedback_display_body("CONTINUE summary\n\nfull review");
        assert_eq!(body, "summary\n\nfull review");
        assert!(!body.contains("CONTINUE"), "verdict word not repeated");
        // A summary-only review keeps its content (details() alone drops it).
        assert_eq!(feedback_display_body("CONTINUE lgtm"), "lgtm");
        // Request-changes with a details body.
        assert_eq!(
            feedback_display_body("REQUEST_CHANGES needs work\n\ndetails here"),
            "needs work\n\ndetails here"
        );
    }

    #[test]
    fn overlay_refresh_keeps_scroll_offset() {
        let data = |subject: &str| CommitDetail {
            sha: crate::lifecycle::CommitSha::parse(&format!("{:0<40}", "abc")).unwrap(),
            short: "abc".into(),
            subject: subject.into(),
            body: "body".into(),
            reviews: Vec::new(),
        };
        let subject_of = |o: &Overlay| match &o.data {
            OverlayData::Commit(d) => d.subject.clone(),
            OverlayData::Plan { .. } => unreachable!(),
        };
        // A review row opens at a non-zero offset (commit row would be 0).
        let mut o = Overlay::commit(data("first"), 5);
        assert_eq!(o.offset, 5, "opened at the review-focus offset");
        // THE bug this guards: a background re-fetch (verdict landed, file
        // churn) swaps the data but must KEEP the reader's scroll position.
        o.refresh(OverlayData::Commit(data("updated")));
        assert_eq!(subject_of(&o), "updated", "data was swapped");
        assert_eq!(o.offset, 5, "scroll offset survives a refresh");
        // Scrolling clamps to [0, max_off].
        o.scroll(100, 8);
        assert_eq!(o.offset, 8, "clamped to the last page");
        o.scroll(-100, 8);
        assert_eq!(o.offset, 0, "clamped at the top");
        // A plan overlay opens at the top.
        let p = Overlay::plan("foo".into(), Some("# foo".into()));
        assert_eq!(p.offset, 0);
    }

    #[test]
    fn read_plan_markdown_prefers_plans_then_finished() {
        let repo = tempfile::TempDir::new().unwrap();
        let p = repo.path();
        std::fs::create_dir_all(p.join(".clank/plans")).unwrap();
        std::fs::create_dir_all(p.join(".clank/finished")).unwrap();
        // Active plan in plans/.
        std::fs::write(p.join(".clank/plans/foo.md"), "# foo active").unwrap();
        assert_eq!(
            read_plan_markdown(p, "foo").as_deref(),
            Some("# foo active")
        );
        // Finalized plan only in finished/.
        std::fs::write(p.join(".clank/finished/bar.md"), "# bar done").unwrap();
        assert_eq!(read_plan_markdown(p, "bar").as_deref(), Some("# bar done"));
        // plans/ wins when both slots exist (an active plan shadows a stale
        // finished copy).
        std::fs::write(p.join(".clank/plans/bar.md"), "# bar active").unwrap();
        assert_eq!(
            read_plan_markdown(p, "bar").as_deref(),
            Some("# bar active")
        );
        // Missing → None (the overlay shows "plan file not found").
        assert!(read_plan_markdown(p, "nope").is_none());
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
    pub(crate) fn detail_repo() -> tempfile::TempDir {
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
}
