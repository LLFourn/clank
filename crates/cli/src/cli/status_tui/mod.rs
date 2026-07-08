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
    /// A raw stdin chunk. Parsed AT THE LOOP, not in the reader
    /// thread: bytes → keys is interpretation and the interpretation
    /// is mode-scoped — an open text input reads the letters
    /// j/k/y/n/o/q as TEXT, which a context-free parse would eat as
    /// commands (tui-plan-actions-page).
    Stdin(Vec<u8>),
}

/// Execute a confirmed roster mutation via the existing `clank agent`
/// cores — the TUI is a FRONT-END to add/remove, never a second write
/// path. The target label is resolved from the snapshot/picker by the
/// action's index at apply time. Adds go in as a commit reviewer
/// (parity with `clank agent add`). A failed core leaves the roster
/// unchanged and the error is RETURNED for the error overlay — silent
/// `let _ =` swallowing was a bug this plan fixes in passing (codex
/// e4bccc5). The mutated `.clank/config.json` is tracked, so this
/// dirties the tree — the deliberate, committed-config change the
/// confirm modal warned about.
fn apply_confirm(
    action: ConfirmAction,
    repo: &std::path::Path,
    home: Option<&std::path::Path>,
    snap: &StatusSnapshot,
    picker: &[crate::cli::status::AvailableAgent],
) -> anyhow::Result<()> {
    match action {
        ConfirmAction::AddCandidate { idx } => {
            if let Some(c) = picker.get(idx)
                && let Ok(label) = clank_core::ids::AgentLabel::parse(&c.label)
            {
                crate::cli::agent::add_repo_roster_agent_by_name(
                    repo,
                    home,
                    &label,
                    crate::cli::teams_config::RosterRole::Commit,
                )?;
            }
            Ok(())
        }
        ConfirmAction::RemoveAgent { idx } => {
            if let Some(a) = snap.agents.get(idx)
                && let Ok(label) = clank_core::ids::AgentLabel::parse(&a.label)
            {
                crate::cli::agent::remove_repo_agent(repo, &label)?;
            }
            Ok(())
        }
        // Plan-page confirms are executed by `run_plan_confirm` in the
        // loop's Confirm arm (they're async and error-reporting); they
        // never reach this sync roster path.
        ConfirmAction::StashPlan | ConfirmAction::PurgeArtifacts | ConfirmAction::PurgeDrop => {
            Ok(())
        }
    }
}

/// Execute a plan-page confirm via the SAME cores the CLI verbs run —
/// the TUI is a front-end, never a reimplemented write. `yes: true`
/// because the TUI's confirm screen IS the confirmation. Purges pass
/// `allow_rewrite_protected: true`: the TUI's chooser + scary confirm
/// is stronger consent than the CLI flag, and dogfooding happens on
/// `master` (reviewers: challenge if you disagree).
async fn run_plan_confirm(
    action: ConfirmAction,
    repo: &std::path::Path,
    stem: &str,
) -> anyhow::Result<()> {
    match action {
        ConfirmAction::StashPlan => {
            crate::cli::stash::run_push(crate::cli::StashPushArgs {
                plan: Some(stem.to_string()),
                waiting_for: None,
                to_queue: false,
                priority: None,
                force: false,
                dry: false,
                yes: true,
                allow_rewrite_protected: true,
                repo: Some(repo.to_path_buf()),
            })
            .await
        }
        ConfirmAction::PurgeArtifacts | ConfirmAction::PurgeDrop => {
            crate::cli::purge::run(crate::cli::PurgeArgs {
                plan: Some(stem.to_string()),
                all: false,
                repo: Some(repo.to_path_buf()),
                into_branch: None,
                dry: false,
                yes: true,
                squash: None,
                amend: false,
                drop: matches!(action, ConfirmAction::PurgeDrop),
                allow_rewrite_protected: true,
                no_cache: false,
            })
            .await
        }
        _ => Ok(()),
    }
}

/// The error overlay's title for a failed plan-page action.
fn plan_confirm_title(action: ConfirmAction) -> String {
    match action {
        ConfirmAction::StashPlan => "stash push failed".to_string(),
        ConfirmAction::PurgeArtifacts => "purge failed".to_string(),
        ConfirmAction::PurgeDrop => "purge --drop failed".to_string(),
        _ => "action failed".to_string(),
    }
}

/// What a plan-input submit did.
enum InputSubmit {
    /// Invalid input (empty subject; drop stem mismatch) — stay open.
    Stay,
    /// Action ran clean — back to the page (refresh reconciles).
    Done,
    /// Action failed — error overlay with (title, message).
    Failed(String, String),
}

/// Execute a submitted plan-page input. Validation lives here so the
/// screens can't diverge from what actually runs: an empty buffer
/// never acts, and DropStem only arms on the EXACT stem.
async fn submit_plan_input(
    kind: PlanInputKind,
    buf: &str,
    plan_page: &Option<PlanPage>,
    snapshot: &StatusSnapshot,
    repo: &std::path::Path,
) -> InputSubmit {
    let Some(pp) = plan_page.as_ref() else {
        return InputSubmit::Done; // page gone — nothing sane to do
    };
    let stem = pp.stem.as_str();
    let text = buf.trim();
    match kind {
        PlanInputKind::ForceFinishSubject => {
            if text.is_empty() {
                return InputSubmit::Stay;
            }
            // The WHY paragraph is auto-provenance: the honest fact is
            // WHO decided (the human, at the TUI) and what the gate
            // said at bypass.
            let gate = snapshot
                .plans
                .iter()
                .find(|p| p.plan.as_str() == stem)
                .map(|p| p.gate.as_str().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let why = format!(
                "force-finished from clank status --tui; the review gate was \
                 `{gate}` at bypass."
            );
            let res = crate::cli::finish::run(crate::cli::FinishArgs {
                plan: Some(stem.to_string()),
                repo: Some(repo.to_path_buf()),
                message: vec![text.to_string(), why],
                purge: false,
                squash: None,
                no_squash: false,
                into_branch: None,
                allow_rewrite_protected: true,
                dry: false,
                force: true,
                no_cache: false,
            })
            .await;
            match res {
                Ok(()) => InputSubmit::Done,
                Err(e) => InputSubmit::Failed("force finish failed".into(), format!("{e:?}")),
            }
        }
        PlanInputKind::SquashMessage => {
            if text.is_empty() {
                return InputSubmit::Stay;
            }
            // finish validates the squash message as a WHAT+WHY (the
            // raw one-liner is rejected unconditionally —
            // tui-squash-message-body). The plan already HAS a WHY:
            // its finalize commit's body; compose it in.
            let finalize_body = match finalize_commit_of(repo, stem).await {
                Some(sha) => crate::git_io::commit_body_at(repo, &sha).unwrap_or_default(),
                None => String::new(),
            };
            let msg = input::compose_squash_message(text, &finalize_body);
            // `finish --squash` on an already-finished plan is the
            // purpose-built path: only the range is rewritten.
            let res = crate::cli::finish::run(crate::cli::FinishArgs {
                plan: Some(stem.to_string()),
                repo: Some(repo.to_path_buf()),
                message: vec![],
                purge: false,
                squash: Some(msg),
                no_squash: false,
                into_branch: None,
                allow_rewrite_protected: true,
                dry: false,
                force: false,
                no_cache: false,
            })
            .await;
            match res {
                Ok(()) => InputSubmit::Done,
                Err(e) => InputSubmit::Failed("squash failed".into(), format!("{e:?}")),
            }
        }
        PlanInputKind::BlockReason => {
            if text.is_empty() {
                return InputSubmit::Stay;
            }
            let Some(master) = snapshot
                .agents
                .iter()
                .find(|a| matches!(a.role, crate::cli::teams_config::RosterRole::Master))
            else {
                return InputSubmit::Failed(
                    "block failed".into(),
                    "no master agent on the roster to author the block".into(),
                );
            };
            // Same core as `clank block create` (the Blocked hook
            // fires identically); the block is authored as the MASTER
            // (blocks live under an agent dir; the master owns the
            // plan). Fixed name `pause`: one TUI pause per plan,
            // idempotent re-block.
            let res = crate::cli::block::run(crate::cli::BlockArgs {
                command: crate::cli::BlockCmd::Create(crate::cli::BlockCreateArgs {
                    name: "pause".to_string(),
                    message: text.to_string(),
                    plan: Some(stem.to_string()),
                    all: false,
                    author: Some(master.label.clone()),
                    repo: Some(repo.to_path_buf()),
                }),
            })
            .await;
            match res {
                Ok(()) => InputSubmit::Done,
                Err(e) => InputSubmit::Failed("block failed".into(), format!("{e:?}")),
            }
        }
        PlanInputKind::DropStem => {
            // RAW buffer, not the trimmed `text`: the gate must be the
            // SAME predicate the armed indicator renders (codex
            // 0daf087 — a trimmed compare here executed drops the
            // screen showed as not armed).
            if !input::drop_armed(buf, stem) {
                return InputSubmit::Stay; // not armed — keep typing or esc
            }
            match run_plan_confirm(ConfirmAction::PurgeDrop, repo, stem).await {
                Ok(()) => InputSubmit::Done,
                Err(e) => InputSubmit::Failed("purge --drop failed".into(), format!("{e:?}")),
            }
        }
    }
}

/// Answer every pending block on the plan: the human clicked unblock,
/// so each creator's block gets the canned answer via the SAME core as
/// `clank unblock` — their `block clean` flow then sweeps the pair.
async fn unblock_plan(
    repo: &std::path::Path,
    stem: &str,
    snapshot: &StatusSnapshot,
) -> anyhow::Result<()> {
    for b in snapshot
        .blocks
        .iter()
        .filter(|b| b.answer.is_none() && b.plan.as_deref() == Some(stem))
    {
        crate::cli::block::run_unblock(crate::cli::UnblockArgs {
            agent: b.agent.clone(),
            name: b.name.clone(),
            plan: Some(stem.to_string()),
            message: "unblocked from clank status --tui".to_string(),
            repo: Some(repo.to_path_buf()),
        })
        .await?;
    }
    Ok(())
}

/// The squash input's prefill: the finalize commit's subject (the
/// message the plan finished with is usually the right squash summary).
/// Any failure degrades to empty — the user types their own.
async fn squash_prefill(repo: &std::path::Path, stem: &str) -> String {
    let Some(finalized_at) = finalize_commit_of(repo, stem).await else {
        return String::new();
    };
    crate::git_io::commit_subject_at(repo, &finalized_at)
        .ok()
        .map(|s| {
            // Strip the `[stem] ` tag — the squash core re-tags.
            s.strip_prefix(&format!("[{stem}] "))
                .unwrap_or(&s)
                .to_string()
        })
        .unwrap_or_default()
}

/// The finished plan's finalize commit, from the cached fold.
async fn finalize_commit_of(
    repo: &std::path::Path,
    stem: &str,
) -> Option<crate::lifecycle::CommitSha> {
    let state = crate::rebuild::rebuild_repo_with_policy(repo, crate::rebuild::CachePolicy::Use)
        .await
        .ok()?;
    let key = crate::lifecycle::PlanKey::parse(stem).ok()?;
    state
        .fold
        .finished_plans
        .iter()
        .find(|f| f.plan == key)
        .map(|f| f.finalized_at.clone())
}

/// The plan page's derived facts, or `None` when the stem no longer
/// resolves to an active OR finished plan (page closes). `multi_commit`
/// (squash-worthiness) for finished plans re-derives the plan's native
/// range via the cached fold — one-shot at page open / refresh, never
/// per repaint.
async fn plan_page_facts(
    repo: &std::path::Path,
    stem: &str,
    snapshot: &StatusSnapshot,
) -> Option<input::PlanPageState> {
    let active = snapshot.plans.iter().any(|p| p.plan.as_str() == stem);
    let finished_file = repo.join(crate::init_facts::finished_md_rel(stem)).exists();
    if !active && !finished_file {
        return None;
    }
    let blocked = active
        && snapshot
            .blocks
            .iter()
            .any(|b| b.answer.is_none() && b.plan.as_deref() == Some(stem));
    let multi_commit = if active {
        true // squash isn't offered on active pages; value unused
    } else {
        finished_multi_commit(repo, stem).await
    };
    Some(input::PlanPageState {
        finished: !active,
        multi_commit,
        blocked,
    })
}

/// Refresh the open plan page's facts in place; `false` = the plan is
/// gone and the page must close (clears `plan_page`, restoring the
/// mode⟺Some invariant at both callers).
async fn refetch_plan_page(
    repo: &std::path::Path,
    plan_page: &mut Option<PlanPage>,
    snapshot: &StatusSnapshot,
) -> bool {
    let Some(pp) = plan_page.as_mut() else {
        return false;
    };
    match plan_page_facts(repo, &pp.stem, snapshot).await {
        Some(st) => {
            pp.st = st;
            // The document shown beneath the buttons may have changed
            // (plan revised, finished) — re-read alongside the facts.
            pp.body = read_plan_markdown(repo, &pp.stem);
            true
        }
        None => {
            *plan_page = None;
            false
        }
    }
}

/// Does the finished plan's own range still span >1 commit? (An
/// autosquashed plan is one commit — squash would be a no-op row.)
/// Errors degrade to `true`: offering a squash that no-ops is better
/// than hiding one that would work.
async fn finished_multi_commit(repo: &std::path::Path, stem: &str) -> bool {
    let Ok(state) =
        crate::rebuild::rebuild_repo_with_policy(repo, crate::rebuild::CachePolicy::Use).await
    else {
        return true;
    };
    let Ok(key) = crate::lifecycle::PlanKey::parse(stem) else {
        return true;
    };
    let Some(fp) = state.fold.finished_plans.iter().find(|f| f.plan == key) else {
        return true;
    };
    match crate::preview::re_fold_finished_plan_natives(repo, &key, &fp.finalized_at).await {
        Ok(natives) => natives.len() > 1,
        Err(_) => true,
    }
}

/// Execute a detail-page action via the existing `clank agent` cores
/// and return the next mode. ToggleAuto and the review checkboxes STAY
/// on the page (the row is updated in place so several boxes can be
/// ticked in one visit; a concurrent external Refresh re-locates the
/// open page by LABEL, so the roster reordering is safe); Promote
/// returns to the panel (the agent's own action set changes shape);
/// Remove defers to the Confirm modal. The TUI is a front-end to the
/// cores, never a reimplemented write.
fn apply_detail_action(
    action: DetailAction,
    idx: usize,
    sel: usize,
    snapshot: &mut StatusSnapshot,
    repo: &std::path::Path,
    error: &mut Option<(String, String)>,
) -> Mode {
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
        DetailAction::TierCommit | DetailAction::TierPlan | DetailAction::TierFinal => {
            if let Some(current) = input::role_review_kind(role)
                && let Some(next) = input::tier_after_toggle(current, action)
                && crate::cli::agent::set_repo_review(repo, &label, next).is_ok()
            {
                snapshot.agents[idx].role = input::review_kind_role(next);
            }
            Mode::AgentDetail { idx, sel }
        }
        DetailAction::PromoteToMaster => match crate::cli::agent::set_repo_master(repo, &label) {
            Ok(()) => Mode::AgentPanel { sel: 0 },
            // Stay on the page and surface the failure — a silently
            // unchanged roster reads as "clank ignored me" (codex
            // e4bccc5).
            Err(e) => {
                *error = Some(("promote failed".to_string(), format!("{e:?}")));
                Mode::AgentDetail { idx, sel }
            }
        },
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

/// THE INVARIANT: panel focus ⟹ the log viewport is at its top. The
/// panel sits ABOVE the log in one continuous column (Up from the log's
/// first entry crosses into it; Down past the panel's last row crosses
/// back), so a focused panel over a mid-scroll log reads as two cursors
/// at once. Enforced HERE on every loop pass — not by per-transition
/// discipline — so no panel entry path (the Up-crossing, Tab's teleport
/// from any depth, a refresh rebind, or any future one) can violate it.
/// Idempotent; a no-op while the log owns focus.
fn enforce_panel_tops_log(mode: Mode, log: &mut LogView) {
    if !matches!(mode, Mode::LogScroll) {
        log.enter_first();
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
    /// Per-file +/− counts vs first parent (display; empty when the
    /// numstat read fails — the overlay still opens).
    stats: Vec<crate::git_io::FileStat>,
}

/// What a full-window document overlay is showing. Each kind carries the
/// identity needed to re-fetch on a background `Refresh` (a commit sha; a
/// plan stem), so the watcher firing never loses the reader's place.
enum OverlayData {
    /// A failed action's error text (no re-fetch identity — the error
    /// is a moment, not a live document; Refresh leaves it as-is).
    Error {
        title: String,
        message: String,
    },
    Commit(CommitDetail),
    /// (An ACTIVE plan's document has no overlay: the plan page renders
    /// it beneath the action buttons — tui-plan-page-redesign.)
    ///
    /// A QUEUED plan's markdown by name (source: `.clank/queue/`, not
    /// `plans/`); `markdown` is `None` when it left the queue.
    QueuedPlan {
        name: String,
        markdown: Option<String>,
    },
    /// A STASHED plan's markdown by name — read from the stash record's
    /// protective ref (the plan file no longer exists on the branch);
    /// `None` when the item left the stash.
    StashedPlan {
        name: String,
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
    /// A queued-plan overlay, opened at the top.
    fn queued(name: String, markdown: Option<String>) -> Self {
        Self {
            data: OverlayData::QueuedPlan { name, markdown },
            offset: 0,
        }
    }
    /// A failed plan-page action's error, opened at the top
    /// (errors-in-the-TUI, tui-plan-actions-page).
    fn error(title: String, message: String) -> Self {
        Self {
            data: OverlayData::Error { title, message },
            offset: 0,
        }
    }
    /// A stashed-plan overlay, opened at the top.
    fn stashed(name: String, markdown: Option<String>) -> Self {
        Self {
            data: OverlayData::StashedPlan { name, markdown },
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
/// Borrow a [`CommitDetail`] as the render layer's [`render::CommitDoc`].
fn commit_doc(d: &CommitDetail) -> render::CommitDoc<'_> {
    render::CommitDoc {
        short_sha: &d.short,
        subject: &d.subject,
        body: &d.body,
        stats: &d.stats,
        reviews: &d.reviews,
    }
}

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
    // Stats degrade to empty on failure — the overlay still opens
    // (subject/body/reviews are the load-bearing content).
    let stats = crate::git_io::commit_numstat_at(repo, sha).unwrap_or_default();
    Some(CommitDetail {
        short: crate::cli::status::short_sha(sha.as_str()).to_string(),
        sha: sha.clone(),
        stats,
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

/// Read a queued plan's markdown by NAME via a fresh queue scan — the
/// priority prefix can change under us (a reprioritise renames the file),
/// so the path is resolved at read time, never cached.
fn read_queue_markdown(repo: &std::path::Path, name: &str) -> Option<String> {
    let entry = crate::cli::queue::scan_queue(repo)
        .into_iter()
        .find(|e| e.name == name)?;
    std::fs::read_to_string(&entry.path).ok()
}

/// Read a stashed plan's markdown by NAME: resolve the item through the
/// merged stash scan and read `plans/<name>.md` from THE RECORD'S OWN
/// protective ref (the ref-path contract) — the file no longer exists on
/// the branch. Re-resolved on refresh; `None` when the item left the
/// stash or the blob is missing.
fn read_stash_markdown(repo: &std::path::Path, name: &str) -> Option<String> {
    let (_, record) = crate::cli::stash::scan_stash(repo)
        .into_iter()
        .find(|(stem, _)| stem == name)?;
    let tip = crate::git_io::resolve_commit(repo, &record.git_ref)?;
    let rel = crate::init_facts::plan_md_rel(name);
    crate::git_io::show_blob(repo, &tip, std::path::Path::new(&rel)).ok()
}

/// What an open overlay can render as HTML — a plan stem or a full commit
/// sha, owned so the overlay's borrow is released before dispatch.
enum HtmlTarget {
    Plan(String),
    Commit(String),
    Queue(String),
    Stash(String),
}

/// Open the overlay's plan/commit as its rendered HTML page in the host
/// browser. Spawns the built `clank` DETACHED with stdio nulled: the TUI
/// owns the terminal (raw/alt-screen), so build progress must not print into
/// the pane, and detaching lets the browser open when the incremental build
/// finishes without freezing the loop. For a commit whose page was evicted
/// from the incremental window, pass `--rebuild` so the open still succeeds.
fn open_overlay_in_browser(repo: &std::path::Path, target: &HtmlTarget) {
    use crate::cli::html::HtmlOpenTarget;
    let (open_target, rebuild) = match target {
        HtmlTarget::Plan(stem) => (HtmlOpenTarget::Plan(stem.as_str()), false),
        HtmlTarget::Commit(sha) => {
            let page = repo.join(format!(".clank/html/commit/{sha}.html"));
            (HtmlOpenTarget::Commit(sha.as_str()), !page.exists())
        }
        // Queue/stash pages are re-rendered on every build, so no
        // --rebuild heuristic is needed — a plain open regenerates the site.
        HtmlTarget::Queue(name) => (HtmlOpenTarget::Queue(name.as_str()), false),
        HtmlTarget::Stash(name) => (HtmlOpenTarget::Stash(name.as_str()), false),
    };
    let argv = crate::cli::html::html_open_argv(repo, open_target, rebuild);
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .args(&argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
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
    input: Vec<u8>,
    refresh: bool,
    resize: bool,
}

/// Fold a drained event burst into a [`Batch`]. Stdin chunks are
/// CONCATENATED — a CSI sequence split across two 16-byte reads
/// reassembles here before parsing. Pure → unit-tested (the
/// "N wakes → one refresh" coalescing guarantee).
fn coalesce(events: impl IntoIterator<Item = Ev>) -> Batch {
    let mut b = Batch {
        input: Vec::new(),
        refresh: false,
        resize: false,
    };
    for ev in events {
        match ev {
            Ev::Stdin(bytes) => b.input.extend(bytes),
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
            if ev_tx.send(Ev::Stdin(buf[..n as usize].to_vec())).is_err() {
                return;
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
    // The open plan-actions page (tui-plan-actions-page): identity +
    // derived facts. INVARIANT: `Some` ⟺ mode is PlanDetail/PurgeChoice/
    // a plan-page Confirm — set and cleared ONLY together with those
    // mode transitions.
    let mut plan_page: Option<PlanPage> = None;
    // The open plan-page text input's buffer (Mode::PlanInput carries
    // only the KIND — same Copy-preserving split as `plan_page`).
    let mut plan_input: Option<TextInput> = None;
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
        // Every pass, before any render: see the fn's invariant doc.
        enforce_panel_tops_log(mode, &mut log);

        // The commit-detail overlay owns the whole pane until dismissed:
        // render it, route its own (scroll / back) keys, and skip the
        // normal log/panel path. Nothing here animates, so the wait is
        // the slow backstop.
        if detail.is_some() {
            let page = (rows as usize).saturating_sub(3).max(1);
            let overlay = detail.as_ref().unwrap();
            let (lines, total) = match &overlay.data {
                OverlayData::Commit(d) => render_commit_detail(
                    &commit_doc(d),
                    overlay.offset,
                    rows as usize,
                    cols as usize,
                ),
                OverlayData::QueuedPlan { name, markdown }
                | OverlayData::StashedPlan { name, markdown } => render_plan_doc(
                    name,
                    markdown.as_deref(),
                    overlay.offset,
                    rows as usize,
                    cols as usize,
                ),
                OverlayData::Error { title, message } => {
                    render_error_doc(title, message, overlay.offset, rows as usize, cols as usize)
                }
            };
            // Extract the html-open target by value up front so the
            // `OpenHtml` arm doesn't hold a borrow of `detail` (the `Back`
            // arm mutates it).
            let html_target: Option<HtmlTarget> = match &overlay.data {
                OverlayData::Commit(d) => Some(HtmlTarget::Commit(d.sha.as_str().to_string())),
                OverlayData::QueuedPlan { name, .. } => Some(HtmlTarget::Queue(name.clone())),
                OverlayData::StashedPlan { name, .. } => Some(HtmlTarget::Stash(name.clone())),
                OverlayData::Error { .. } => None,
            };
            paint(&lines);
            match ev_rx.recv_timeout(Duration::from_secs(60)) {
                Ok(Ev::Stdin(bytes)) => {
                    for k in parse_keys(&bytes) {
                        match doc_nav(k, page) {
                            DocNav::Back => detail = None,
                            DocNav::Scroll(delta) => {
                                let max_off = total.saturating_sub((rows as usize).max(1));
                                if let Some(d) = detail.as_mut() {
                                    d.scroll(delta, max_off);
                                }
                            }
                            DocNav::OpenHtml => {
                                if let Some(t) = &html_target {
                                    open_overlay_in_browser(&repo, t);
                                }
                            }
                            DocNav::None => {}
                        }
                        if detail.is_none() {
                            break; // dismissed — remaining keys route next pass
                        }
                    }
                }
                // Data changed under us — re-fetch by IDENTITY (commit sha /
                // plan stem), but KEEP the scroll offset (refresh swaps
                // data, not view-state).
                Ok(Ev::Refresh) => {
                    let new = match &detail.as_ref().unwrap().data {
                        OverlayData::Commit(d) => {
                            let sha = d.sha.clone();
                            fetch_commit_detail(&repo, &sha).map(OverlayData::Commit)
                        }
                        OverlayData::QueuedPlan { name, .. } => {
                            let name = name.clone();
                            Some(OverlayData::QueuedPlan {
                                markdown: read_queue_markdown(&repo, &name),
                                name,
                            })
                        }
                        OverlayData::StashedPlan { name, .. } => {
                            let name = name.clone();
                            Some(OverlayData::StashedPlan {
                                markdown: read_stash_markdown(&repo, &name),
                                name,
                            })
                        }
                        // An error is a moment, not a live document —
                        // a background refresh must not clear or morph
                        // it while the user reads.
                        OverlayData::Error { .. } => None,
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
            plan_page: plan_page.as_ref(),
            plan_input: plan_input.as_ref(),
            picker: &picker,
            log_cursor: log.cursor,
        };
        let capacity = render_at(&snapshot, rows, cols, log.offset, frame, &view).1;

        // The ask lines depend on blocks (not the log fetch), so compute
        // them before filling. `head` is the count of scrollable rows that
        // aren't log rows — ONLY the ask lines: in-progress activity lives
        // on the AGENTS panel rows, not in the scroll sequence, so it must
        // not count here (codex 5482f26: phantom rows put the cursor past
        // the rendered sequence). INVARIANT: head + log_rows.len() ==
        // build_scroll(...).len(), the same sequence render and
        // Enter-targeting walk — pinned by
        // scroll_sequence_is_pure_history_no_placeholders.
        let ask_lines = block_ask_spans(&snapshot, cols as usize);
        let in_prog = in_progress_rows(&snapshot);
        let head = ask_lines.len();

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
            plan_page: plan_page.as_ref(),
            plan_input: plan_input.as_ref(),
            picker: &picker,
            log_cursor: log.cursor,
        };
        // `screen_total` is the current screen's virtual content height
        // (log rows; plan page chrome + full document) — the plan page's
        // body-scroll clamp reads it, mirroring the overlay pattern.
        let (screen, screen_total) = render_at(&snapshot, rows, cols, log.offset, frame, &view);
        paint(&screen);

        // The spinner lives on the AGENTS panel rows, which are always on
        // screen when a roster exists — tick whenever any agent is active
        // (no window math; the log carries no placeholders).
        let spinner_visible = !in_prog.is_empty() && !snapshot.agents.is_empty();
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
                // Incremental parse: ONE key at a time, interpretation
                // picked from the CURRENT mode — a pasted `f<subject>⏎`
                // crosses into input mode mid-buffer and its text must
                // not be pre-parsed as commands.
                let mut inbuf = batch.input.as_slice();
                loop {
                    let text_mode = matches!(mode, Mode::PlanInput { .. });
                    let Some((anykey, used)) = parse_one(inbuf, text_mode) else {
                        if inbuf.is_empty() {
                            break;
                        }
                        inbuf = &inbuf[1..];
                        continue;
                    };
                    inbuf = &inbuf[used..];
                    // Text keys route to the ONE input arm; command keys
                    // to the per-mode match below.
                    let k = match anykey {
                        AnyKey::Text(tk) => {
                            if let Mode::PlanInput { kind } = mode {
                                let outcome = plan_input
                                    .as_mut()
                                    .map(|ti| text_input_nav(ti, tk))
                                    .unwrap_or(InputNav::Cancel);
                                match outcome {
                                    InputNav::Cancel => {
                                        plan_input = None;
                                        mode = Mode::PlanDetail { sel: 0 };
                                    }
                                    InputNav::Submit => {
                                        let buf = plan_input
                                            .as_ref()
                                            .map(|ti| ti.buf.clone())
                                            .unwrap_or_default();
                                        match submit_plan_input(
                                            kind, &buf, &plan_page, &snapshot, &repo,
                                        )
                                        .await
                                        {
                                            InputSubmit::Stay => {}
                                            InputSubmit::Done => {
                                                plan_input = None;
                                                mode = Mode::PlanDetail { sel: 0 };
                                                refresh_pending = true;
                                            }
                                            InputSubmit::Failed(title, msg) => {
                                                plan_input = None;
                                                detail = Some(Overlay::error(title, msg));
                                                mode = Mode::PlanDetail { sel: 0 };
                                                refresh_pending = true;
                                            }
                                        }
                                    }
                                    InputNav::None => {}
                                }
                            }
                            continue;
                        }
                        AnyKey::Cmd(k) => k,
                    };
                    let page = (rows as usize).saturating_sub(3).max(1);
                    // `mode` is Copy: matching it copies, so reassigning `mode`
                    // inside an arm is free of borrow conflicts. The per-mode
                    // routing is PURE (agent_panel_action / agent_detail_nav /
                    // confirm_decision); the loop only executes the result
                    // (where IO happens).
                    match mode {
                        Mode::AgentPanel { sel } => {
                            match agent_panel_action(
                                sel,
                                &snapshot.agents,
                                snapshot.stash.len(),
                                snapshot.queue.len(),
                                k,
                            ) {
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
                                PanelAction::OpenQueueItem(q) => {
                                    if let Some(item) = snapshot.queue.get(q) {
                                        let name = item.name.clone();
                                        let md = read_queue_markdown(&repo, &name);
                                        detail = Some(Overlay::queued(name, md));
                                    }
                                }
                                PanelAction::OpenQueueHtml(q) => {
                                    if let Some(item) = snapshot.queue.get(q) {
                                        open_overlay_in_browser(
                                            &repo,
                                            &HtmlTarget::Queue(item.name.clone()),
                                        );
                                    }
                                }
                                PanelAction::OpenStashItem(i) => {
                                    if let Some(item) = snapshot.stash.get(i) {
                                        let name = item.stem.clone();
                                        let md = read_stash_markdown(&repo, &name);
                                        detail = Some(Overlay::stashed(name, md));
                                    }
                                }
                                PanelAction::OpenStashHtml(i) => {
                                    if let Some(item) = snapshot.stash.get(i) {
                                        open_overlay_in_browser(
                                            &repo,
                                            &HtmlTarget::Stash(item.stem.clone()),
                                        );
                                    }
                                }
                                PanelAction::NudgeQueue { idx, delta } => {
                                    if let Some(item) = snapshot.queue.get(idx) {
                                        let name = item.name.clone();
                                        let next = (item.priority as i32 + delta as i32)
                                            .clamp(0, 999)
                                            as u16;
                                        // ONE validated mutation — the same
                                        // primitive `queue reprioritise` uses,
                                        // never an inline rename.
                                        if crate::cli::queue::set_priority(&repo, &name, next)
                                            .is_ok()
                                        {
                                            snapshot.queue[idx].priority = next;
                                            snapshot.queue.sort_by(|a, b| {
                                                a.priority
                                                    .cmp(&b.priority)
                                                    .then(a.name.cmp(&b.name))
                                            });
                                            // Keep the cursor ON the nudged
                                            // item as it moves through the
                                            // re-sorted list.
                                            if let Some(pos) =
                                                snapshot.queue.iter().position(|i| i.name == name)
                                            {
                                                mode = Mode::AgentPanel {
                                                    sel: snapshot.agents.len() + 1 + pos,
                                                };
                                            }
                                        }
                                    }
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
                                    let mut err: Option<(String, String)> = None;
                                    mode = apply_detail_action(
                                        action,
                                        idx,
                                        sel,
                                        &mut snapshot,
                                        &repo,
                                        &mut err,
                                    );
                                    if let Some((title, msg)) = err {
                                        detail = Some(Overlay::error(title, msg));
                                    }
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
                            | Key::Right
                            | Key::Yes
                            | Key::No
                            | Key::Html
                            | Key::Plus
                            | Key::Minus
                            | Key::Char(_) => {}
                        },
                        // Confirm: one decision, resolved in one place.
                        Mode::Confirm { action } => {
                            if let Some(go) = confirm_decision(action, k) {
                                if action.is_plan_page() {
                                    // Plan-page confirms execute the async
                                    // core here (the TUI confirm IS the
                                    // confirmation — the core's own prompt
                                    // is bypassed with yes:true); an Err
                                    // opens the error overlay instead of
                                    // vanishing (errors-in-the-TUI).
                                    if go && let Some(pp) = plan_page.as_ref() {
                                        if let Err(e) =
                                            run_plan_confirm(action, &repo, &pp.stem).await
                                        {
                                            detail = Some(Overlay::error(
                                                plan_confirm_title(action),
                                                format!("{e:?}"),
                                            ));
                                        }
                                        // Success or failure, the repo may
                                        // have changed — rebuild; the rebind
                                        // exits the page if the plan is gone.
                                        refresh_pending = true;
                                    }
                                    mode = Mode::PlanDetail { sel: 0 };
                                    continue;
                                }
                                if go
                                    && let Err(e) = apply_confirm(
                                        action,
                                        &repo,
                                        home.as_deref(),
                                        &snapshot,
                                        &picker,
                                    )
                                {
                                    detail = Some(Overlay::error(
                                        "roster change failed".to_string(),
                                        format!("{e:?}"),
                                    ));
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
                        // Unreachable by construction: PlanInput parses
                        // with text_mode=true, so every key routed here
                        // is a TextKey handled above — but the match
                        // must stay exhaustive.
                        Mode::PlanInput { .. } => {}
                        // The plan-actions page (tui-plan-actions-page).
                        Mode::PlanDetail { sel } => {
                            if k == Key::Quit {
                                break 'evloop;
                            }
                            let Some(pp) = plan_page.clone() else {
                                mode = Mode::LogScroll; // invariant breach — bail out
                                continue;
                            };
                            let actions = plan_actions(pp.st);
                            match plan_detail_nav(sel, &actions, k, page, pp.scroll) {
                                PlanNav::Sel(x) => mode = Mode::PlanDetail { sel: x },
                                PlanNav::Scroll(delta) => {
                                    if let Some(live) = plan_page.as_mut() {
                                        let max_off =
                                            screen_total.saturating_sub((rows as usize).max(1));
                                        live.scroll = (live.scroll as i64 + delta as i64)
                                            .clamp(0, max_off as i64)
                                            as usize;
                                    }
                                }
                                PlanNav::Back => {
                                    plan_page = None;
                                    mode = Mode::LogScroll;
                                }
                                PlanNav::Act(a) => match a {
                                    PlanAction::OpenHtml => open_overlay_in_browser(
                                        &repo,
                                        &HtmlTarget::Plan(pp.stem.clone()),
                                    ),
                                    PlanAction::Stash => {
                                        mode = Mode::Confirm {
                                            action: ConfirmAction::StashPlan,
                                        };
                                    }
                                    PlanAction::Purge => mode = Mode::PurgeChoice { sel: 0 },
                                    PlanAction::Back => {
                                        plan_page = None;
                                        mode = Mode::LogScroll;
                                    }
                                    PlanAction::ForceFinish => {
                                        plan_input = Some(TextInput::default());
                                        mode = Mode::PlanInput {
                                            kind: PlanInputKind::ForceFinishSubject,
                                        };
                                    }
                                    PlanAction::Squash => {
                                        let prefill = squash_prefill(&repo, &pp.stem).await;
                                        plan_input = Some(TextInput::prefilled(&prefill));
                                        mode = Mode::PlanInput {
                                            kind: PlanInputKind::SquashMessage,
                                        };
                                    }
                                    PlanAction::Block => {
                                        plan_input = Some(TextInput::default());
                                        mode = Mode::PlanInput {
                                            kind: PlanInputKind::BlockReason,
                                        };
                                    }
                                    // Unblock needs no input: the HUMAN is
                                    // the one clicking, so the pending
                                    // block(s) get a canned answer and the
                                    // creator's block-clean flow stays
                                    // intact.
                                    PlanAction::Unblock => {
                                        if let Err(e) =
                                            unblock_plan(&repo, &pp.stem, &snapshot).await
                                        {
                                            detail = Some(Overlay::error(
                                                "unblock failed".to_string(),
                                                format!("{e:?}"),
                                            ));
                                        }
                                        refresh_pending = true;
                                    }
                                },
                                PlanNav::None => {}
                            }
                        }
                        // The purge chooser: artifacts-only vs drop.
                        Mode::PurgeChoice { sel } => {
                            if k == Key::Quit {
                                break 'evloop;
                            }
                            match purge_choice_nav(sel, k) {
                                PlanNavPurge::Sel(x) => mode = Mode::PurgeChoice { sel: x },
                                PlanNavPurge::Choose(PurgeChoice::Artifacts) => {
                                    mode = Mode::Confirm {
                                        action: ConfirmAction::PurgeArtifacts,
                                    };
                                }
                                PlanNavPurge::Choose(PurgeChoice::Drop) => {
                                    // The scariest gate: type the stem to
                                    // arm (a y/N would be one habitual
                                    // keystroke from losing real work).
                                    plan_input = Some(TextInput::default());
                                    mode = Mode::PlanInput {
                                        kind: PlanInputKind::DropStem,
                                    };
                                }
                                PlanNavPurge::Choose(PurgeChoice::Back) => {
                                    mode = Mode::PlanDetail { sel: 0 };
                                }
                                PlanNavPurge::None => {}
                            }
                        }
                        // Default: the log is a selectable timeline — keys move
                        // the CURSOR entry (the viewport follows via
                        // scroll_to_show at the loop top). `Up` on the first
                        // entry crosses back into the panel (continuous nav).
                        Mode::LogScroll => match k {
                            Key::Quit => break 'evloop,
                            Key::Focus => mode = mode.toggle_focus(snapshot.agents.len()),
                            Key::Up => {
                                match log_up_target(log.cursor, log.offset, snapshot.agents.len()) {
                                    Some(sel) => mode = Mode::AgentPanel { sel },
                                    None => log.up(),
                                }
                            }
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
                                let seq = build_scroll(&snapshot, &ask_lines);
                                match entry_overlay_target(&seq, log.cursor) {
                                    Some(OverlayTarget::Commit { sha, focus }) => {
                                        if let Some(data) = fetch_commit_detail(&repo, &sha) {
                                            let offset = match &focus {
                                                Some(author) => commit_review_offset(
                                                    &commit_doc(&data),
                                                    author,
                                                    cols as usize,
                                                ),
                                                None => 0,
                                            };
                                            detail = Some(Overlay::commit(data, offset));
                                        }
                                    }
                                    // A plan header opens the ACTIONS
                                    // page (buttons over the document —
                                    // tui-plan-page-redesign).
                                    Some(OverlayTarget::Plan { stem }) => {
                                        if let Some(st) =
                                            plan_page_facts(&repo, &stem, &snapshot).await
                                        {
                                            let body = read_plan_markdown(&repo, &stem);
                                            plan_page = Some(PlanPage {
                                                stem,
                                                st,
                                                body,
                                                scroll: 0,
                                            });
                                            mode = Mode::PlanDetail { sel: 0 };
                                        }
                                    }
                                    None => {}
                                }
                            }
                            Key::Escape
                            | Key::Left
                            | Key::Right
                            | Key::Delete
                            | Key::Yes
                            | Key::No
                            | Key::Html
                            | Key::Plus
                            | Key::Minus
                            | Key::Char(_) => {}
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
                // Same identity rule for a panel cursor on a QUEUE row: a
                // reprioritise renames the queue file, so THIS refresh is
                // often self-inflicted and re-sorts the list — capture the
                // selected item's NAME so the cursor follows it.
                let (panel_stash_name, panel_queue_name) = match mode {
                    Mode::AgentPanel { sel } if sel > snapshot.agents.len() => {
                        let i = sel - snapshot.agents.len() - 1;
                        if i < snapshot.stash.len() {
                            (snapshot.stash.get(i).map(|s| s.stem.clone()), None)
                        } else {
                            (
                                None,
                                snapshot
                                    .queue
                                    .get(i - snapshot.stash.len())
                                    .map(|q| q.name.clone()),
                            )
                        }
                    }
                    _ => (None, None),
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
                    // The plan page doesn't depend on the roster — it
                    // must survive an empty-roster refresh (its arms are
                    // below); everything agent-shaped drops to the log.
                    Mode::AgentPanel { .. } | Mode::AgentDetail { .. } | Mode::AddPicker { .. }
                        if snapshot.agents.is_empty() =>
                    {
                        Mode::LogScroll
                    }
                    Mode::Confirm { action }
                        if snapshot.agents.is_empty() && !action.is_plan_page() =>
                    {
                        Mode::LogScroll
                    }
                    Mode::AgentPanel { sel } => Mode::AgentPanel {
                        sel: rebind_panel_sel(
                            sel,
                            panel_stash_name.as_deref(),
                            panel_queue_name.as_deref(),
                            snapshot.agents.len(),
                            &snapshot.stash,
                            &snapshot.queue,
                        ),
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
                    // The plan page tracks its plan by STEM: recompute
                    // the facts against the fresh snapshot (block state /
                    // finished-ness can flip under us); a vanished plan
                    // (stashed, purged, dropped) closes the page to the
                    // log. The chooser and plan-page confirms collapse
                    // back to the page (their target may have changed).
                    Mode::PlanDetail { sel } => {
                        match refetch_plan_page(&repo, &mut plan_page, &snapshot).await {
                            true => Mode::PlanDetail { sel },
                            false => Mode::LogScroll,
                        }
                    }
                    Mode::PurgeChoice { .. } => {
                        match refetch_plan_page(&repo, &mut plan_page, &snapshot).await {
                            true => Mode::PlanDetail { sel: 0 },
                            false => Mode::LogScroll,
                        }
                    }
                    // A half-typed input SURVIVES a background refresh
                    // (the watcher fires constantly in a live session —
                    // clearing it would eat the user's text); it closes
                    // only when its plan vanished.
                    Mode::PlanInput { kind } => {
                        match refetch_plan_page(&repo, &mut plan_page, &snapshot).await {
                            true => Mode::PlanInput { kind },
                            false => {
                                plan_input = None;
                                Mode::LogScroll
                            }
                        }
                    }
                    Mode::Confirm { action } if action.is_plan_page() => {
                        match refetch_plan_page(&repo, &mut plan_page, &snapshot).await {
                            true => Mode::PlanDetail { sel: 0 },
                            false => Mode::LogScroll,
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
            Ev::Stdin(b"j".to_vec()),
            Ev::Refresh,
            Ev::Resize,
            Ev::Refresh,
        ]);
        assert!(b.refresh, "many Refresh fold to one flag");
        assert!(b.resize, "Resize folded");
        assert_eq!(b.input, b"j".to_vec(), "stdin bytes preserved in order");
        // Chunks CONCATENATE — a CSI split across two 16-byte reads
        // reassembles before parsing.
        let split = coalesce(vec![Ev::Stdin(b"\x1b[".to_vec()), Ev::Stdin(b"A".to_vec())]);
        assert_eq!(
            parse_keys(&split.input),
            vec![Key::Up],
            "split escape sequence reassembled"
        );
        // 100 wakes still yield a single refresh flag and no spurious input.
        let many = coalesce((0..100).map(|_| Ev::Refresh));
        assert!(many.refresh);
        assert!(many.input.is_empty());
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
            stats: Vec::new(),
            sha: crate::lifecycle::CommitSha::parse(&format!("{:0<40}", "abc")).unwrap(),
            short: "abc".into(),
            subject: subject.into(),
            body: "body".into(),
            reviews: Vec::new(),
        };
        let subject_of = |o: &Overlay| match &o.data {
            OverlayData::Commit(d) => d.subject.clone(),
            OverlayData::QueuedPlan { .. }
            | OverlayData::StashedPlan { .. }
            | OverlayData::Error { .. } => unreachable!(),
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
        // A queued-plan overlay opens at the top.
        let p = Overlay::queued("foo".into(), Some("# foo".into()));
        assert_eq!(p.offset, 0);
    }

    #[test]
    fn panel_focus_forces_the_log_to_its_top() {
        // THE INVARIANT: any panel-family mode ⟹ log topped. A wheel
        // burst used to cross into the panel mid-batch, freezing a
        // mid-scroll viewport under the focused panel; Tab could do the
        // same from any depth. The loop-pass enforcement makes the state
        // unrepresentable regardless of which path entered the panel.
        let mut log = LogView::new();
        log.cursor = 7;
        log.offset = 5;
        enforce_panel_tops_log(Mode::AgentPanel { sel: 0 }, &mut log);
        assert_eq!((log.cursor, log.offset), (0, 0), "panel focus tops the log");

        // Every panel-family mode enforces; the log mode never touches it.
        for mode in [
            Mode::AddPicker { sel: 0 },
            Mode::AgentDetail { idx: 0, sel: 0 },
            Mode::Confirm {
                action: ConfirmAction::RemoveAgent { idx: 0 },
            },
        ] {
            let mut log = LogView::new();
            log.cursor = 3;
            log.offset = 3;
            enforce_panel_tops_log(mode, &mut log);
            assert_eq!((log.cursor, log.offset), (0, 0), "{mode:?}");
        }
        let mut log = LogView::new();
        log.cursor = 7;
        log.offset = 5;
        enforce_panel_tops_log(Mode::LogScroll, &mut log);
        assert_eq!(
            (log.cursor, log.offset),
            (7, 5),
            "log focus keeps its scroll state"
        );
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
    fn apply_detail_action_tier_checkbox_uses_the_core_and_stays_on_page() {
        use crate::cli::teams_config::RosterRole;
        let repo = detail_repo();
        let mut s = two_agent_snap(); // idx 1 == codex (commit)
        // Tick `plan` on a commit-tier reviewer → leaves commit mode.
        let next =
            apply_detail_action(DetailAction::TierPlan, 1, 2, &mut s, repo.path(), &mut None);
        let parsed: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            parsed["agents"]["codex"]["role"], "plan",
            "commit → plan via set_repo_review"
        );
        assert!(
            matches!(next, Mode::AgentDetail { idx: 1, sel: 2 }),
            "a tier edit STAYS on the detail page: {next:?}"
        );
        assert_eq!(
            s.agents[1].role,
            RosterRole::Plan,
            "snapshot row updated in place so the checkboxes repaint live"
        );

        // Tick `final` too → plan+final == gate.
        let next = apply_detail_action(
            DetailAction::TierFinal,
            1,
            3,
            &mut s,
            repo.path(),
            &mut None,
        );
        let parsed: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            parsed["agents"]["codex"]["role"], "gate",
            "plan+final persists as gate"
        );
        assert!(matches!(next, Mode::AgentDetail { idx: 1, sel: 3 }));

        // Unticking down to the last coverage is a no-op (still gate→final
        // →final stays; final is the only box left, untick refused).
        apply_detail_action(DetailAction::TierPlan, 1, 2, &mut s, repo.path(), &mut None);
        let before = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        apply_detail_action(
            DetailAction::TierFinal,
            1,
            3,
            &mut s,
            repo.path(),
            &mut None,
        );
        let after = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        assert_eq!(before, after, "unticking the last coverage is a no-op");
    }

    #[test]
    fn apply_detail_action_failed_promote_surfaces_the_error_and_stays() {
        // codex e4bccc5: set_repo_master failures were `let _ =`
        // swallowed — a silently unchanged roster reads as "clank
        // ignored me". A failing core (no repo at the path) must
        // surface through the error out-param and keep the page open.
        let mut s = two_agent_snap();
        let mut err: Option<(String, String)> = None;
        let next = apply_detail_action(
            DetailAction::PromoteToMaster,
            1,
            4,
            &mut s,
            std::path::Path::new("/nonexistent/clank-test-repo"),
            &mut err,
        );
        let (title, msg) = err.expect("failure must be reported, not swallowed");
        assert_eq!(title, "promote failed");
        assert!(!msg.is_empty());
        assert_eq!(
            next,
            Mode::AgentDetail { idx: 1, sel: 4 },
            "stay on the page so the user sees where they were"
        );
    }

    #[test]
    fn apply_detail_action_promote_uses_the_core_and_demotes_old_master() {
        let repo = detail_repo();
        let mut s = two_agent_snap();
        apply_detail_action(
            DetailAction::PromoteToMaster,
            1,
            0,
            &mut s,
            repo.path(),
            &mut None,
        );
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
        let next = apply_detail_action(
            DetailAction::ToggleAuto,
            1,
            2,
            &mut s,
            repo.path(),
            &mut None,
        );
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
        let next = apply_detail_action(DetailAction::Remove, 1, 3, &mut s, repo.path(), &mut None);
        assert_eq!(
            next,
            Mode::Confirm {
                action: ConfirmAction::RemoveAgent { idx: 1 }
            }
        );
    }
}
