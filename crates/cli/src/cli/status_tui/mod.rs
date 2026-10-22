//! `clank tui` — full-screen, READ-ONLY, live-updating
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

mod event_content;
pub(crate) mod lease;
pub(crate) mod text;
mod wait_detail;
mod wait_process;
// Re-export for `open_zellij`'s tab/pane renamer, which strips the same
// stale lamp prefix. (The IO shell itself uses no text primitives — the
// view modules do.)
pub(crate) use text::strip_leading_emoji;

mod block_page;
mod derive;

mod markdown;

pub(crate) mod input;
use input::*;

mod remote;
mod remote_owner;
mod render;
use render::*;

mod scroll;
use scroll::*;

mod zellij;

#[cfg(test)]
pub(crate) mod fixtures;

// ── terminal plumbing ───────────────────────────────────────
// The raw-mode alt-screen lifecycle, the size probe, and the frame
// painter live in `super::term`.

/// What wakes the TUI loop.
enum Ev {
    /// A status/data change — probe the signature; rebuild iff it moved.
    Refresh,
    /// A terminal resize (SIGWINCH). NOT a data change: top up the
    /// (possibly taller) viewport and repaint, but never rebuild — so a
    /// resize is never swallowed by the nothing-changed gate.
    Resize,
    /// A github body fetch finished (tui-github-event-content). Its
    /// generation and target are carried back so the page can refuse
    /// a completion it no longer wants.
    Content {
        generation: u64,
        target: clank_core::wait::ContentRef,
        state: event_content::ContentState,
    },
    /// One bounded process read completed; the request carries the full
    /// attendance/process identity and generation needed to reject stragglers.
    Process(wait_process::Completion),
    /// A raw stdin chunk. Parsed AT THE LOOP, not in the reader
    /// thread: bytes → keys is interpretation and the interpretation
    /// is mode-scoped — an open text input reads the letters
    /// j/k/y/n/o/q as TEXT, which a context-free parse would eat as
    /// commands (tui-plan-actions-page).
    Stdin(Vec<u8>),
    /// The zellij worker answered a reopen request. Carries nothing:
    /// every loop turn drains the worker's outcomes before painting,
    /// so the event only has to end the wait.
    Worker,
    /// The remote server spoke or ended; drained the same way.
    Remote,
}

/// Execute a confirmed roster mutation via the existing `clank agent`
/// cores — the TUI is a FRONT-END to add/remove, never a second write
/// path. The target label is resolved from the snapshot/picker by the
/// action's index at apply time. Adds go in as a commit reviewer
/// (parity with `clank agent add`). A failed core leaves the roster
/// unchanged and the error is RETURNED for the error overlay — silent
/// `let _ =` swallowing was a bug this plan fixes in passing (codex
/// e4bccc5). The mutated `.clank/config.json` is local-only repo
/// state; the confirm modal names that local write explicitly.
/// The panel's cursor positions for this snapshot at this width.
///
/// THE source of panel positions. Every transition that names a row
/// goes through here: computing one arithmetically works only while
/// drawn rows and cursor positions correspond one to one, and a wait
/// row above the target breaks that silently (codex on bc033f0).
fn panel_row_list(
    snap: &StatusSnapshot,
    cols: usize,
) -> Vec<crate::cli::status_tui::input::PanelRow> {
    crate::cli::status_tui::input::panel_rows(
        snap.agents.len(),
        &crate::cli::status_tui::render::drawable_waits(snap, cols),
        snap.stash.len(),
        snap.queue.len(),
        !snap.drafts.is_empty(),
        input::source_groups(&snap.sources).len(),
        !input::active_timers(&snap.sources).is_empty(),
    )
}

/// Where `row` sits in the panel, or the "+ add" row when it is gone.
fn row_position(
    rows: &[crate::cli::status_tui::input::PanelRow],
    row: crate::cli::status_tui::input::PanelRow,
) -> usize {
    rows.iter()
        .position(|r| *r == row)
        .unwrap_or_else(|| crate::cli::status_tui::input::add_row_index(rows))
}

/// Where the open wait page's attendance lives NOW in the roster, or
/// `None` if that attendance is gone.
///
/// Located by INSTANCE — agent label, task, `at`, PID and birth — never by the
/// index it had when the page opened. A roster edit reorders the list
/// under the cursor, and this repo has already paid once for trusting
/// an index across a refresh.
fn relocate_wait(
    page: &crate::cli::status_tui::input::WaitPage,
    agents: &[crate::cli::status::AgentAutoRow],
) -> Option<usize> {
    agents.iter().position(|a| {
        a.label == page.label
            && a.attending
                .as_ref()
                .is_some_and(|att| page.matches(&a.label, att))
    })
}

/// Snapshot refresh and repaint only retain or invalidate a request; they
/// NEVER request another inspection. Reordering relocates the same instance.
fn reconcile_wait_process(
    mode: &mut Mode,
    page: &mut Option<WaitPage>,
    agents: &[crate::cli::status::AgentAutoRow],
    process: &mut wait_process::Controller,
) {
    if !matches!(
        mode,
        Mode::WaitDetail { .. }
            | Mode::Confirm {
                action: ConfirmAction::KillAttended
            }
    ) {
        *page = None;
        process.close();
        return;
    }
    match page.as_ref().and_then(|p| relocate_wait(p, agents)) {
        Some(agent) => {
            if let Mode::WaitDetail { sel, .. } = *mode {
                *mode = Mode::WaitDetail { agent, sel };
            }
        }
        None => {
            *page = None;
            process.close();
            *mode = Mode::AgentPanel { sel: 0 };
        }
    }
}

fn pump_wait_process(process: &mut wait_process::Controller, tx: &mpsc::Sender<Ev>) {
    let Some(request) = process.take_job() else {
        return;
    };
    let wake = tx.clone();
    if let Err(error) = wait_process::spawn(
        request.clone(),
        crate::proc_identity::inspect::inspect,
        move |completion| {
            let _ = wake.send(Ev::Process(completion));
        },
    ) {
        // Thread creation failed: release the reservation and report an
        // unavailable observation. No retry or render-loop native read.
        process.complete(wait_process::Completion {
            command: None,
            request,
            inspection: crate::proc_identity::inspect::Inspection::Unavailable(
                crate::proc_identity::inspect::Unavailable::Io(error.kind()),
            ),
        });
    }
}

/// The pid a confirmed kill may signal, or why it may not.
///
/// Reads the attendance record FROM DISK rather than from the TUI's
/// snapshot. The snapshot is refreshed by an asynchronous filesystem
/// wake, so it can lag the file by however long the confirm was on
/// screen; validating against it would let a record replaced after the
/// confirm opened still pass (codex on f2793af). Taking no snapshot
/// argument is what makes that mistake unavailable here.
///
/// Re-run AFTER the confirm, never when the page opened: the record
/// can be replaced, the process can exit and its pid be reused, all
/// while the answer is pending.
fn resolve_kill_target(
    page: &crate::cli::status_tui::input::WaitPage,
    repo: &std::path::Path,
) -> Result<crate::signal::Pid, String> {
    // Removing an agent leaves its local `attended` file behind, so
    // the file alone is not authority to signal: an agent taken off
    // the roster while the confirm was open would still be killable
    // (codex on bc033f0). Roster membership is read here, freshly,
    // for the same reason the record is.
    match crate::agent_store::load_repo_config_required(repo) {
        Ok(cfg) => {
            if !cfg.agents.keys().any(|l| l.as_str() == page.label.as_str()) {
                return Err(format!("`{}` is no longer on the roster", page.label));
            }
        }
        Err(e) => return Err(format!("cannot read the roster: {e}")),
    }
    let Some(att) = crate::cli::agent_state::attended_now(repo, &page.label) else {
        return Err(format!("`{}` is no longer attending anything", page.label));
    };
    if !page.matches(&page.label, &att) {
        return Err("that wait was replaced by a different one".to_string());
    }
    att.killable_pid().ok_or_else(|| {
        att.kill_blocker()
            .unwrap_or("it cannot be identified")
            .to_string()
    })
}

fn apply_confirm(
    action: ConfirmAction,
    repo: &std::path::Path,
    home: Option<&std::path::Path>,
    snap: &StatusSnapshot,
    picker: &[crate::cli::status::AvailableAgent],
) -> anyhow::Result<()> {
    match action {
        // Executed at the call site, which holds the page identity the
        // post-confirm revalidation needs.
        ConfirmAction::KillAttended
        | ConfirmAction::ClearSession { .. }
        | ConfirmAction::RestartSource => Ok(()),
        ConfirmAction::AddCandidate { idx, tier } => {
            if let Some(c) = picker.get(idx)
                && let Ok(label) = clank_core::ids::AgentLabel::parse(&c.label)
            {
                crate::cli::agent::add_repo_roster_agent_by_name(
                    repo,
                    home,
                    &label,
                    crate::cli::teams_config::RosterRole::from(tier),
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
        ConfirmAction::StashPlan
        | ConfirmAction::UnqueuePlan
        | ConfirmAction::PromotePlan
        | ConfirmAction::QueueDraft
        | ConfirmAction::PromoteDraft
        | ConfirmAction::RestorePlan
        | ConfirmAction::DropStashedPlan { .. }
        | ConfirmAction::PurgeArtifacts
        | ConfirmAction::PurgeDrop { .. } => Ok(()),
    }
}

/// Execute a plan-page confirm via the SAME cores the CLI verbs run —
/// the TUI is a front-end, never a reimplemented write. The TUI's
/// confirm screen is its own affordance; the cores ask nothing.
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
                dry: false,
                repo: Some(repo.to_path_buf()),
            })
            .await
        }
        ConfirmAction::UnqueuePlan => crate::cli::queue::unqueue(repo, stem).map(|_| ()),
        ConfirmAction::PromotePlan => crate::cli::queue::promote(repo, stem),
        ConfirmAction::QueueDraft => crate::cli::queue::queue_draft(repo, stem).map(|_| ()),
        ConfirmAction::PromoteDraft => crate::cli::queue::promote_draft(repo, stem),
        ConfirmAction::RestorePlan => {
            crate::cli::stash::run_pop_inner(Some(repo), Some(stem)).await
        }
        // Warnings are DISCARDED, not printed: stdout belongs to the
        // alternate screen, and the core's own warning is the same
        // sentence the confirm already made the operator read.
        // The fingerprint the SCREEN showed travels to the delete,
        // which refuses a replacement. Verifying here instead would
        // reopen the window it exists to close.
        //
        // Warnings are discarded, not printed: stdout belongs to the
        // alternate screen, and the core's warning is the sentence the
        // confirm already made the operator read.
        ConfirmAction::DropStashedPlan { identity, .. } => {
            crate::cli::stash::run_drop_checked(Some(repo), Some(stem), Some(identity), &mut |_| {})
                .await
        }
        ConfirmAction::PurgeArtifacts | ConfirmAction::PurgeDrop { .. } => {
            crate::cli::purge::run(crate::cli::PurgeArgs {
                plan: Some(stem.to_string()),
                all: false,
                repo: Some(repo.to_path_buf()),
                into_branch: None,
                dry: false,
                squash: None,
                amend: false,
                drop: matches!(action, ConfirmAction::PurgeDrop { .. }),
                no_cache: false,
            })
            .await
        }
        _ => Ok(()),
    }
}

/// The draft under the drafts list's cursor, by file. The list is
/// ordered by modification time, so a refresh can move it: this is what
/// the cursor follows.
fn selected_draft(mode: Mode, snapshot: &StatusSnapshot) -> Option<std::path::PathBuf> {
    match mode {
        Mode::DraftsList { sel } => snapshot.drafts.get(sel).map(|d| d.path.clone()),
        _ => None,
    }
}

/// How long until the soonest active timer comes due, so an idle pane
/// shows it owed when that happens, with nothing on disk changing. A
/// second past the due time, so the paint lands after it.
fn until_next_due(snapshot: &StatusSnapshot, now: i64) -> Option<Duration> {
    snapshot
        .sources
        .iter()
        .filter_map(|s| match s.timer {
            Some(crate::cli::timer_source::TimerState::Due(at)) => Some(at as i64),
            _ => None,
        })
        .filter(|at| *at > now)
        .min()
        .map(|at| Duration::from_secs((at - now) as u64 + 1))
}

/// The timer under the timers list's cursor, by its owner and log, for
/// the cursor to follow across a refresh that reorders the list.
fn selected_timer(mode: Mode, snapshot: &StatusSnapshot) -> Option<input::SourceRef> {
    match mode {
        Mode::TimersList { sel } => input::active_timers(&snapshot.sources)
            .get(sel)
            .map(|s| input::SourceRef::of(s)),
        _ => None,
    }
}

/// The member under a source row's member list cursor, by its owner and
/// log, for the cursor to follow across a refresh.
fn selected_member(
    mode: Mode,
    group: Option<&input::GroupRef>,
    snapshot: &StatusSnapshot,
) -> Option<input::SourceRef> {
    match (mode, group) {
        (Mode::SourceMembers { sel }, Some(group)) => group
            .members(&snapshot.sources)
            .get(sel)
            .map(|s| input::SourceRef::of(s)),
        _ => None,
    }
}

/// What Enter on the panel's source row `row` opens: a source only one
/// agent configures opens its page; one several do lists them. Each is
/// `(source_page, source_group, mode)`.
fn open_source_row(
    row: usize,
    sources: &[crate::cli::source_status::ConfiguredSource],
) -> Option<(Option<input::SourceRef>, Option<input::GroupRef>, Mode)> {
    let group = input::source_groups(sources).get(row)?.clone();
    Some(match group.members(sources)[..] {
        [only] => (
            Some(input::SourceRef::of(only)),
            None,
            Mode::SourcePage { sel: 0, lift: 0 },
        ),
        _ => (None, Some(group), Mode::SourceMembers { sel: 0 }),
    })
}

/// Where a source's Back lands: its member list when it was opened from
/// one, else the panel on its row.
fn source_back(
    source: &crate::cli::source_status::ConfiguredSource,
    group: Option<&input::GroupRef>,
    snapshot: &StatusSnapshot,
    cols: usize,
) -> Mode {
    if source.timer.is_some() {
        return Mode::TimersList {
            sel: input::active_timers(&snapshot.sources)
                .iter()
                .position(|s| *s == source)
                .unwrap_or(0),
        };
    }
    match group {
        Some(group) => Mode::SourceMembers {
            sel: group
                .members(&snapshot.sources)
                .iter()
                .position(|s| *s == source)
                .unwrap_or(0),
        },
        None => source_row_in_panel(&input::GroupRef::of(source), snapshot, cols),
    }
}

/// The panel, its cursor on `group`'s row.
fn source_row_in_panel(group: &input::GroupRef, snapshot: &StatusSnapshot, cols: usize) -> Mode {
    let rows = panel_row_list(snapshot, cols);
    Mode::AgentPanel {
        sel: input::source_groups(&snapshot.sources)
            .iter()
            .position(|g| g == group)
            .map(|i| row_position(&rows, input::PanelRow::Source(i)))
            .unwrap_or_else(|| input::add_row_index(&rows)),
    }
}

/// A draft's page, its document read from its own file; its Back
/// returns to `row` of the drafts list.
fn draft_page(draft: &crate::cli::queue::Draft, row: usize) -> PlanPage {
    PlanPage {
        body: std::fs::read_to_string(&draft.path).ok(),
        stem: draft.name.clone(),
        st: input::PlanPageState::Draft,
        entry: Some(draft.path.clone()),
        scroll: 0,
        drafts_row: Some(row),
    }
}

/// A stashed plan's page, its document read from the stash.
fn stash_page(repo: &std::path::Path, stem: &str) -> PlanPage {
    PlanPage {
        body: read_stash_markdown(repo, stem),
        stem: stem.to_string(),
        st: input::PlanPageState::Stashed,
        entry: None,
        scroll: 0,
        drafts_row: None,
    }
}

/// What a plan-page confirm's outcome does to the open page. Restoring,
/// queueing a draft and promoting one move the plan's source: a refresh
/// still reading the one it left would close the page.
fn settle_plan_confirm(
    repo: &std::path::Path,
    action: ConfirmAction,
    outcome: &anyhow::Result<()>,
    page: &mut Option<PlanPage>,
) {
    let (Ok(()), Some(pp)) = (outcome, page.as_mut()) else {
        return;
    };
    match action {
        ConfirmAction::RestorePlan | ConfirmAction::PromoteDraft => {
            pp.st = input::PlanPageState::Active { repo_paused: false };
            pp.entry = None;
        }
        ConfirmAction::QueueDraft => {
            if let Some(queued) = crate::cli::queue::scan_queue(repo)
                .into_iter()
                .find(|e| e.name == pp.stem && e.priority == crate::cli::queue::ADD_PRIORITY)
            {
                pp.st = input::PlanPageState::Queued {
                    priority: queued.priority,
                };
                pp.entry = Some(queued.path);
            }
        }
        _ => {}
    }
}

/// The error overlay's title for a failed plan-page action.
fn plan_confirm_title(action: ConfirmAction) -> String {
    match action {
        ConfirmAction::StashPlan => "stash push failed".to_string(),
        ConfirmAction::UnqueuePlan => "unqueue failed".to_string(),
        ConfirmAction::PromotePlan | ConfirmAction::PromoteDraft => "promote failed".to_string(),
        ConfirmAction::QueueDraft => "queue failed".to_string(),
        ConfirmAction::RestorePlan => "stash pop failed".to_string(),
        ConfirmAction::DropStashedPlan { .. } => "stash drop failed".to_string(),
        ConfirmAction::PurgeArtifacts => "purge failed".to_string(),
        ConfirmAction::PurgeDrop { .. } => "purge --drop failed".to_string(),
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

/// How many commits a drop of `stem` would take, asked of the same
/// preview the purge engine uses — so the screen's number is the
/// number that will be acted on, for an ACTIVE plan and a FINISHED
/// one alike.
///
/// The fold cannot answer for a finished plan: it keeps a plan's
/// commit list only while the plan is active, and reading the absence
/// as `0` told the operator a drop would take nothing (codex on
/// 3ed8a39).
///
/// FALLIBLE, and the caller must not open the screen without an
/// answer. The preview's failure is the one a test can force; the
/// rebuild's `?` is error propagation on principle rather than proven
/// behaviour, because `rebuild_repo_with_policy` answers for anything
/// a directory can be. A failure here is not "zero commits": `run_plan_confirm`
/// reruns the rebuild and the preview after `y`, so a read that fails
/// while the screen opens and succeeds a moment later would drop real
/// commits after advertising none (codex on 230585f). No number, no
/// screen.
async fn drop_size(repo: &std::path::Path, stem: &str) -> anyhow::Result<usize> {
    let plan_key = crate::lifecycle::PlanKey::parse(stem)
        .map_err(|e| anyhow::anyhow!("`{stem}` is not a plan name: {e}"))?;
    let state = crate::rebuild::rebuild_repo_with_policy(repo, crate::rebuild::CachePolicy::Use)
        .await
        .map_err(|e| anyhow::anyhow!("reading the repo failed: {e}"))?;
    let preview = crate::preview::build_rewrite_preview(repo, &state, &plan_key, true)
        .await
        .map_err(|e| anyhow::anyhow!("working out what a drop would take failed: {e}"))?;
    Ok(preview.commits.len())
}

/// Execute a submitted plan-page input. Validation lives here so the
/// screens can't diverge from what actually runs: an empty buffer
/// never acts.
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
                .active_plan
                .as_ref()
                .filter(|p| p.plan.as_str() == stem)
                .map(|p| p.gate.as_str().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let why = format!(
                "force-finished from clank tui; the review gate was \
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
            let msg =
                input::compose_squash_message(text, &finalize_body, input::TUI_SQUASH_PROVENANCE);
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
    }
}

/// Create the repo's pause block from the typed question.
///
/// Repo-scoped, so it takes no plan: a pause parks the whole repo and
/// must be raisable when no plan is open or none exists.
async fn submit_pause_question(
    buf: &str,
    snapshot: &StatusSnapshot,
    repo: &std::path::Path,
) -> InputSubmit {
    let text = buf.trim();
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
    // fires identically); authored as the MASTER because
    // blocks live under an agent dir. Fixed name `pause`
    // keeps re-block idempotent, and blocks are repo-wide,
    // so there is exactly one.
    let res = crate::cli::block::run(crate::cli::BlockArgs {
        command: crate::cli::BlockCmd::Create(crate::cli::BlockCreateArgs {
            name: block_page::PAUSE.to_string(),
            message: text.to_string(),
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

/// What one key on the block page leaves the loop to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageStep {
    To(Mode),
    /// The answer was recorded; the page is closed and the blocks are
    /// stale.
    Answered(Mode),
    Quit,
}

/// One key on the open block page. The page answers the ask it holds,
/// through `answer`, judged against the live `snapshot.blocks`; leaving
/// closes it and returns to where it was opened.
fn drive_block_page(
    slot: &mut Option<block_page::BlockPage>,
    key: AnyKey,
    snapshot: &StatusSnapshot,
    rows: usize,
    cols: usize,
    answer: impl FnOnce(&block_page::Ask, &str) -> anyhow::Result<()>,
) -> PageStep {
    let Some(page) = slot.as_mut() else {
        return PageStep::To(Mode::LogScroll);
    };
    let standing = block_page::standing(&page.ask, &snapshot.blocks);
    let layout = block_page_layout(page, standing, rows, cols);
    let nav = page.key(
        key,
        &snapshot.blocks,
        layout.max_scroll(),
        layout.viewport.max(1),
        answer,
    );
    let back = |slot: &mut Option<block_page::BlockPage>| {
        let origin = slot.take().map(|p| p.origin);
        origin_mode(origin.as_ref(), snapshot, cols)
    };
    match nav {
        block_page::Nav::Stay => PageStep::To(Mode::BlockPage),
        block_page::Nav::Quit => PageStep::Quit,
        block_page::Nav::Leave => PageStep::To(back(slot)),
        block_page::Nav::Answered => PageStep::Answered(back(slot)),
    }
}

/// The surface a block page was opened from, as it stands now.
fn origin_mode(
    origin: Option<&block_page::Origin>,
    snapshot: &StatusSnapshot,
    cols: usize,
) -> Mode {
    match origin {
        Some(block_page::Origin::Agent(label)) if !snapshot.agents.is_empty() => {
            let rows = panel_row_list(snapshot, cols);
            let sel = rows
                .iter()
                .position(|r| {
                    matches!(r, PanelRow::Agent(i)
                        if snapshot.agents.get(*i).is_some_and(|a| &a.label == label))
                })
                .unwrap_or(0);
            Mode::AgentPanel { sel }
        }
        _ => Mode::LogScroll,
    }
}

/// What Enter on agent `idx`'s panel row opens: its question while it
/// is asking one, its own page otherwise.
fn agent_row_enter(idx: usize, snapshot: &StatusSnapshot) -> (Mode, Option<block_page::BlockPage>) {
    match snapshot
        .agents
        .get(idx)
        .and_then(|a| block_page::from_agent(&a.label, &snapshot.blocks))
    {
        Some(page) => (Mode::BlockPage, Some(page)),
        None => (Mode::AgentDetail { idx, sel: 0 }, None),
    }
}

/// Answer the TUI's own `pause` block — the one this screen created.
///
/// Agent-authored blocks are deliberately NOT swept here. Each is a
/// distinct question from a distinct agent, and answering them in bulk
/// would write one canned string underneath all of them; that is a
/// fabricated answer, multiplied. They are answered individually from
/// the blocks section.
async fn resume_from_pause(
    repo: &std::path::Path,
    snapshot: &StatusSnapshot,
) -> anyhow::Result<()> {
    for b in snapshot
        .blocks
        .iter()
        .filter(|b| b.answer.is_none() && b.name == block_page::PAUSE)
    {
        crate::cli::block::run_unblock(crate::cli::UnblockArgs {
            agent: b.agent.clone(),
            name: b.name.clone(),
            message: "resumed from clank tui".to_string(),
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
/// The page's facts, read from the SOURCE it opened on: the queue for
/// a queued page, the plans for an active/finished one — never the
/// other, since a queue entry may share a stem with an active plan
/// (a-queued-plan-has-a-page). `None` = gone from that source.
async fn plan_page_facts(
    repo: &std::path::Path,
    stem: &str,
    snapshot: &StatusSnapshot,
    source: PlanSource<'_>,
) -> Option<(input::PlanPageState, Option<std::path::PathBuf>)> {
    match source {
        PlanSource::Queue(entry) => {
            let item = input::resolve_queued(&snapshot.queue, stem, entry, |q| {
                (q.name.as_str(), q.path.as_path())
            })?;
            return Some((
                input::PlanPageState::Queued {
                    priority: item.priority,
                },
                Some(item.path.clone()),
            ));
        }
        PlanSource::Stash => {
            return snapshot
                .stash
                .iter()
                .any(|s| s.stem == stem)
                .then_some((input::PlanPageState::Stashed, None));
        }
        PlanSource::Draft(path) => {
            return snapshot
                .drafts
                .iter()
                .any(|d| d.path == path)
                .then(|| (input::PlanPageState::Draft, Some(path.to_path_buf())));
        }
        PlanSource::Plans => {}
    }
    if !has_plan_page(repo, stem, snapshot) {
        return None;
    }
    if snapshot
        .active_plan
        .as_ref()
        .is_some_and(|p| p.plan.as_str() == stem)
    {
        let repo_paused = block_page::repo_paused(&snapshot.blocks);
        return Some((input::PlanPageState::Active { repo_paused }, None));
    }
    Some((
        input::PlanPageState::Finished {
            multi_commit: finished_multi_commit(repo, stem).await,
        },
        None,
    ))
}

/// Whether `stem` is an active or finished plan: what a page opened
/// from the plans can show.
fn has_plan_page(repo: &std::path::Path, stem: &str, snapshot: &StatusSnapshot) -> bool {
    snapshot
        .active_plan
        .as_ref()
        .is_some_and(|p| p.plan.as_str() == stem)
        || repo.join(crate::init_facts::finished_md_rel(stem)).exists()
}

/// What `o` opens for a queued page: the html page keyed by its FILE
/// stem (`999-foo`), which is what the generator writes per entry.
/// The parsed name is not a handle — two files can share it — so a
/// queued page with no entry opens nothing rather than a page that
/// may be another file's. `None` for an active/finished page.
fn queue_html_handle(page: &PlanPage) -> Option<String> {
    if !page.st.is_queued() {
        return None;
    }
    let entry = page.entry.as_deref()?;
    Some(entry.file_stem()?.to_string_lossy().into_owned())
}

/// A value-row keystroke on a queued page: ONE validated mutation —
/// the primitive `queue reprioritise` uses, never an inline rename —
/// and the page follows the write: its priority, and its identity,
/// since the rename moved the file (a later refresh resolves by that
/// path; left behind, a duplicate-named survivor would close the page
/// instead). `set_priority` refuses an ambiguous name rather than
/// pick, and the error is the page's to show. No-op on a page that is
/// not queued.
/// Write a queued plan's priority and keep the open page's copy of it
/// in step. The page is the only place the value is shown, so a write
/// that did not update it would leave the pane lying until a refresh.
fn set_priority_to(repo: &std::path::Path, page: &mut PlanPage, value: u16) -> anyhow::Result<()> {
    let input::PlanPageState::Queued { .. } = page.st else {
        return Ok(());
    };
    let renamed = crate::cli::queue::set_priority(repo, &page.stem, value)?;
    page.st = input::PlanPageState::Queued { priority: value };
    page.entry = Some(renamed);
    Ok(())
}

/// Where a plan page reads its plan from: the source its state names,
/// in the form the facts lookup takes.
enum PlanSource<'a> {
    Plans,
    Queue(&'a std::path::Path),
    Stash,
    Draft(&'a std::path::Path),
}

impl PlanPage {
    fn source(&self) -> PlanSource<'_> {
        match (self.st, self.entry.as_deref()) {
            (input::PlanPageState::Queued { .. }, Some(entry)) => PlanSource::Queue(entry),
            (input::PlanPageState::Draft, Some(entry)) => PlanSource::Draft(entry),
            (input::PlanPageState::Stashed, _) => PlanSource::Stash,
            _ => PlanSource::Plans,
        }
    }
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
    match plan_page_facts(repo, &pp.stem, snapshot, pp.source()).await {
        Some((st, entry)) => {
            pp.st = st;
            // The document shown beneath the buttons may have changed
            // (plan revised, finished, re-queued) — re-read alongside
            // the facts, from the same source and the same file. An
            // external rename the resolver followed lands here too.
            pp.body = match (&entry, st) {
                (Some(path), _) => std::fs::read_to_string(path).ok(),
                (None, input::PlanPageState::Stashed) => read_stash_markdown(repo, &pp.stem),
                (None, _) => read_plan_markdown(repo, &pp.stem),
            };
            pp.entry = entry;
            true
        }
        None => {
            *plan_page = None;
            false
        }
    }
}

/// Retained-set retargeting (tui-github-event-page): follow the
/// FIRST retained key still present in the fresh events (first-key
/// wins across component splits), refresh the retained set from the
/// newly targeted component, close only when none survive. Pure over
/// the fresh event list — the retarget contract's test seam.
/// What a retarget decided: where the cursor ends up, and whether a
/// new body request must be spawned.
struct Retarget {
    sel: usize,
    /// (repo slug, object, generation) when the page now points at a
    /// DIFFERENT object than it was fetching.
    fetch: Option<(String, clank_core::wait::ContentRef, u64)>,
}

/// Follow the page through a refresh: retarget it, decide whether the
/// body must be re-requested, and rebind the cursor.
///
/// Extracted from the loop because the ORDER is the whole contract and
/// loop code cannot be tested (codex on 4390f27): the action list must
/// be captured BEFORE the retarget replaces `event`, or an
/// availability change made by the refresh itself — a legacy row
/// gaining a URL, say — has already shifted the list by the time the
/// "before" snapshot is taken, and the rebind silently moves the
/// operator's cursor to a different action.
///
/// `None` means the component is gone and the page should close.
fn retarget_event_page(
    page: &mut Option<EventPage>,
    events: &[crate::cli::github::timeline::MergedEvent],
    repo: &std::path::Path,
    sel: usize,
    next_generation: &mut u64,
) -> Option<Retarget> {
    // FIRST — before anything mutates the page.
    let before = page.as_ref().map(actions_for).unwrap_or_default();
    if !refetch_event_page(page, events) {
        return None;
    }
    let ep = page.as_mut()?;
    ep.prompts = event_prompts(repo, &ep.event.clone());

    // A refresh re-requests ONLY when the page now points at a
    // DIFFERENT object: re-fetching on every refresh would re-issue
    // continuously while a failure persists (Contract 4) and would
    // discard a body already on screen.
    let now = ep
        .target
        .member()
        .and_then(|key| crate::cli::github::timeline::resolve_content_ref(&ep.event.members, key))
        .cloned();
    let mut fetch = None;
    if content_needs_refetch(ep.content.as_ref().map(|c| &c.target), now.as_ref()) {
        ep.content = now.map(|target| {
            *next_generation += 1;
            fetch = Some((ep.event.source.clone(), target.clone(), *next_generation));
            event_content::ContentSlot::requesting(target, *next_generation)
        });
    }
    let after = actions_for(ep);
    Some(Retarget {
        sel: rebind_event_sel(&before, sel, &after),
        fetch,
    })
}

/// Whether a refresh must issue a NEW request.
///
/// Only a change of object does. This is the whole of Contract 4's
/// "refreshes never re-issue a failed fetch": the TUI refreshes on
/// every watcher event, so returning true for an unchanged target
/// would re-request continuously for as long as a failure persists,
/// spending the rate limit of the token the wake sources poll with.
/// It would also throw away a body already on screen.
fn content_needs_refetch(
    current: Option<&clank_core::wait::ContentRef>,
    now: Option<&clank_core::wait::ContentRef>,
) -> bool {
    current != now
}

/// Start a body fetch for `target`, answering on `ev_tx`.
///
/// Spawned rather than awaited: attach and refold already make the
/// loop's turn long, and a GitHub round trip must not be added to it.
/// The generation rides along so a slow answer can be refused if the
/// page has moved on.
/// A source page's rows for `source` in this snapshot.
fn source_rows_in(
    source: &crate::cli::source_status::ConfiguredSource,
    snapshot: &StatusSnapshot,
) -> Vec<input::SourceRow> {
    input::source_rows(
        render::restartable(source, &snapshot.sources_live),
        &render::source_events(source, &snapshot.github_events),
    )
}

/// What a source page's cursor is on before a refresh.
fn selected_source_row(
    mode: Mode,
    source: Option<&input::SourceRef>,
    snapshot: &StatusSnapshot,
) -> Option<input::SourcePick> {
    let Mode::SourcePage { sel, .. } = mode else {
        return None;
    };
    let source = source?.find(&snapshot.sources)?;
    input::source_pick(
        &source_rows_in(source, snapshot),
        sel,
        &snapshot.github_events,
    )
}

/// What a confirmed restart asks the monitor: this page's source, as
/// long as it still runs the configuration the confirm was shown.
fn restart_request(
    source: Option<&input::SourceRef>,
    accepted: Option<String>,
) -> Option<crate::cli::monitor::Control> {
    let source = source?;
    Some(crate::cli::monitor::Control::Restart {
        author: source.agent.clone(),
        key: source.key.clone(),
        accepted: accepted?,
    })
}

/// The page of `event`, its content requested.
fn open_event_page(
    repo: &std::path::Path,
    event: &crate::cli::github::timeline::MergedEvent,
    content_generation: &mut u64,
    ev_tx: &mpsc::Sender<Ev>,
) -> Option<EventPage> {
    let first = event.page_keys().into_iter().next()?;
    let want = first
        .member()
        .and_then(|key| crate::cli::github::timeline::resolve_content_ref(&event.members, key))
        .cloned();
    let content = want.map(|target| {
        *content_generation += 1;
        spawn_content_fetch(
            ev_tx,
            event.source.clone(),
            target.clone(),
            *content_generation,
        );
        event_content::ContentSlot::requesting(target, *content_generation)
    });
    Some(EventPage {
        target: first,
        retained: event.page_keys(),
        event: event.clone(),
        prompts: event_prompts(repo, event),
        scroll: 0,
        content,
    })
}

fn spawn_content_fetch(
    ev_tx: &mpsc::Sender<Ev>,
    repo: String,
    target: clank_core::wait::ContentRef,
    generation: u64,
) {
    let tx = ev_tx.clone();
    tokio::spawn(async move {
        let fetcher = event_content::SessionFetch(crate::cli::github::session::shared_session());
        let state = event_content::fetch_content(&fetcher, &repo, &target).await;
        let _ = tx.send(Ev::Content {
            generation,
            target,
            state,
        });
    });
}

fn refetch_event_page(
    page: &mut Option<EventPage>,
    events: &[crate::cli::github::timeline::MergedEvent],
) -> bool {
    let Some(p) = page.as_mut() else {
        return false;
    };
    for key in &p.retained {
        if let Some(ev) = events.iter().find(|e| e.has_page_key(key)) {
            p.target = key.clone();
            p.retained = ev.page_keys();
            p.event = ev.clone();
            return true;
        }
    }
    *page = None;
    false
}

/// The page's standing prompts: each member's watch prompt (the
/// per-agent presentation-time join), deduplicated — one
/// unattributed line when every member agrees, per-agent attribution
/// when they differ (tui-github-event-page).
fn event_prompts(
    repo: &std::path::Path,
    event: &crate::cli::github::timeline::MergedEvent,
) -> Vec<(Option<String>, String)> {
    use std::collections::BTreeSet;
    let mut per_agent: std::collections::HashMap<
        String,
        std::collections::HashMap<String, String>,
    > = Default::default();
    let mut pairs: BTreeSet<(String, String)> = Default::default();
    for m in &event.members {
        let map = per_agent.entry(m.key.agent.clone()).or_insert_with(|| {
            let dir = crate::agent_store::agents_root(repo)
                .join(&m.key.agent)
                .join("events");
            let githubs: Vec<clank_core::agent_config::GithubSource> =
                clank_core::ids::AgentLabel::parse(&m.key.agent)
                    .ok()
                    .and_then(|l| {
                        crate::agent_store::load_agent_config(repo, &l)
                            .ok()
                            .flatten()
                    })
                    .map(|c| {
                        c.wait_events
                            .iter()
                            .filter_map(|s| match s {
                                clank_core::agent_config::WaitEventSource::Github(g) => {
                                    Some(g.clone())
                                }
                                _ => None,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            crate::cli::events::join_prompts(&githubs, &dir)
        });
        if let Some(p) = map.get(&m.key.source) {
            pairs.insert((m.key.agent.clone(), p.clone()));
        }
    }
    let texts: BTreeSet<&String> = pairs.iter().map(|(_, t)| t).collect();
    if texts.len() <= 1 {
        texts.into_iter().map(|t| (None, t.clone())).collect()
    } else {
        pairs.into_iter().map(|(a, t)| (Some(a), t)).collect()
    }
}

/// Apply a refresh rebuild to the loop state — THE resilience seam
/// (tui-transient-refresh-resilience). Success replaces the snapshot
/// (fresh log rows included) and returns true: the caller advances
/// the accepted signature. Failure KEEPS the previous frame (fresh
/// history rows still apply — they're derived independently), mounts
/// ONE transient notice row at the top (replacing a prior refresh
/// notice, never stacking), and returns false: the signature is NOT
/// advanced, so the same input state stays retryable on the next
/// wake. The INITIAL build has no frame to keep and stays fatal at
/// its own call site.
fn apply_refresh(
    snapshot: &mut crate::cli::status::StatusSnapshot,
    fresh_log_rows: Vec<crate::cli::log::OnelineRow>,
    result: Result<crate::cli::status::StatusSnapshot, String>,
    consecutive_failures: &mut u32,
) -> bool {
    use crate::cli::log::OnelineRow;
    const PREFIX: &str = "status refresh failed";
    match result {
        Ok(next) => {
            *consecutive_failures = 0;
            *snapshot = next;
            snapshot.log_rows = fresh_log_rows;
            true
        }
        Err(e) => {
            *consecutive_failures += 1;
            snapshot.log_rows = fresh_log_rows;
            let streak = if *consecutive_failures > 1 {
                format!(" — {} in a row", consecutive_failures)
            } else {
                String::new()
            };
            snapshot.log_rows.insert(
                0,
                OnelineRow::Notice(format!(
                    "{PREFIX}: {e}{streak}; kept the last view, retrying on the next change"
                )),
            );
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
    let Some(fp) = state.fold.finished_record(&key) else {
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
/// An agent's facts as a document: the command it is launched with,
/// whole, and whether a session is bound to it.
pub(super) fn agent_facts(agent: &crate::cli::status::AgentAutoRow) -> String {
    let session = match agent.session.as_ref() {
        Some(bound) => format!("session     {}", bound.id.as_str()),
        None => format!(
            "session     unbound — run `clank as {}` in its session",
            agent.label
        ),
    };
    format!(
        "tool        {}\n\ninvocation\n{}\n\n{session}",
        agent.tool, agent.invocation
    )
}

fn apply_detail_action(
    action: DetailAction,
    idx: usize,
    sel: usize,
    snapshot: &mut StatusSnapshot,
    repo: &std::path::Path,
    // An overlay this action wants opened. Not an "error": a row may
    // want to SHOW something, and the failure case is one of those
    // rather than the only one.
    open: &mut Option<Overlay>,
    // Where this agent's ROW sits in the panel. Not `idx`: a wait
    // above it makes those different numbers, and Back would land on
    // whatever now occupies the index (codex on a561012).
    agent_row: usize,
) -> Mode {
    // Copy out what we need so the &mut write below doesn't conflict.
    let (auto_mode, role, label_str) = match snapshot.agents.get(idx) {
        Some(a) => (a.auto_mode, a.role, a.label.clone()),
        None => return Mode::AgentPanel { sel: 0 },
    };
    let Ok(label) = clank_core::ids::AgentLabel::parse(&label_str) else {
        return Mode::AgentPanel { sel: agent_row };
    };
    match action {
        DetailAction::ToggleAuto => {
            let next = flip_auto(auto_mode);
            if crate::agent_store::set_auto_mode(repo, &label, next).is_ok() {
                snapshot.agents[idx].auto_mode = next;
                snapshot.agents[idx].auto_mode_explicit = Some(next);
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
        // Swap needs a SECOND operand, so it opens a picker rather
        // than acting. Nothing is written until a candidate is chosen.
        DetailAction::Swap => Mode::SwapPicker { out: idx, sel: 0 },
        DetailAction::PromoteToMaster => match crate::cli::agent::set_repo_master(repo, &label) {
            Ok(()) => Mode::AgentPanel { sel: 0 },
            // Stay on the page and surface the failure — a silently
            // unchanged roster reads as "clank ignored me" (codex
            // e4bccc5).
            Err(e) => {
                *open = Some(Overlay::error(
                    "promote failed".to_string(),
                    format!("{e:?}"),
                ));
                Mode::AgentDetail { idx, sel }
            }
        },
        DetailAction::Remove => Mode::Confirm {
            action: ConfirmAction::RemoveAgent { idx },
        },
        DetailAction::ClearSession => {
            unreachable!("the loop opens the clear confirm through open_clear_confirm")
        }
        // The request itself goes to the zellij worker at the call
        // site (this function holds no worker); the page stays open
        // and the answer lands in the notice row.
        DetailAction::Info => {
            *open = Some(Overlay::info(
                format!("agent · {label_str}"),
                agent_facts(&snapshot.agents[idx]),
            ));
            Mode::AgentDetail { idx, sel }
        }
        DetailAction::Reopen => Mode::AgentDetail { idx, sel },
        DetailAction::Back => Mode::AgentPanel { sel: agent_row },
    }
}

/// Release the session the clear confirm named, and return to that
/// agent's page whatever happened.
///
/// Compare-and-clear through [`crate::agent_store::release_binding`]
/// against the target captured when the confirm opened, so a binding
/// that changed after it was shown is left alone, and the page says so.
fn clear_target_session(
    repo: &std::path::Path,
    snapshot: &mut StatusSnapshot,
    target: Option<&crate::cli::status_tui::input::ClearTarget>,
    open: &mut Option<Overlay>,
) -> Mode {
    let not_cleared = |why: String| Some(Overlay::error("nothing was cleared".to_string(), why));
    let Some(target) = target else {
        *open = not_cleared("the confirm had no session to clear".to_string());
        return Mode::AgentPanel { sel: 0 };
    };
    let Some(idx) = relocate_detail(&target.label, &snapshot.agents) else {
        *open = not_cleared(format!("`{}` is no longer on the roster", target.label));
        return Mode::AgentPanel { sel: 0 };
    };
    let page = Mode::AgentDetail { idx, sel: 0 };
    let label = match clank_core::ids::AgentLabel::parse(&target.label) {
        Ok(label) => label,
        Err(e) => {
            *open = not_cleared(format!("{e}"));
            return page;
        }
    };
    let id = &target.session.id;
    match crate::agent_store::release_binding(repo, &label, id) {
        Ok(true) => snapshot.agents[idx].session = None,
        Ok(false) => {
            *open = not_cleared(format!(
                "`{label}` is no longer bound to {}: its binding changed after the confirm \
                 showed it",
                id.as_str()
            ))
        }
        Err(e) => {
            *open = Some(Overlay::error(
                "clear session failed".to_string(),
                format!("{e:?}"),
            ))
        }
    }
    page
}

/// The screens whose content is an agent's pane presence: the roster
/// rows and an agent's page.
fn mode_shows_agents(mode: &Mode) -> bool {
    matches!(mode, Mode::AgentPanel { .. } | Mode::AgentDetail { .. })
}

/// Mount a one-line notice at the top of the log rows, replacing any
/// earlier one with the same `prefix` — like the refresh notice, it
/// never stacks, and the next successful refresh clears it.
fn mount_notice(rows: &mut Vec<crate::cli::log::OnelineRow>, prefix: &str, text: &str) {
    use crate::cli::log::OnelineRow;
    rows.retain(|r| !matches!(r, OnelineRow::Notice(n) if n.starts_with(prefix)));
    rows.insert(0, OnelineRow::Notice(format!("{prefix}: {text}")));
}

/// Deliver every reopen answer, and say whether the loop must go
/// round again to paint one.
///
/// A failure the asker is still positioned to read becomes the
/// overlay; everything else becomes the log notice. The return is the
/// repaint: the overlay's own render branch is at the top of the
/// loop, so mounting one here without re-entering leaves it unseen
/// until something else wakes the pane.
fn mount_reopen_outcomes(
    outcomes: Vec<(String, zellij::ReopenOutcome)>,
    mode: Mode,
    snap: &mut StatusSnapshot,
    detail: &mut Option<Overlay>,
) -> bool {
    let mut mounted = false;
    for (label, outcome) in outcomes {
        if reopen_overlay_for(&label, &outcome, mode, snap) {
            *detail = Some(Overlay::error(
                format!("reopen {label}"),
                outcome.describe(&label),
            ));
            mounted = true;
        } else {
            mount_notice(&mut snap.log_rows, REOPEN_NOTICE, &outcome.describe(&label));
        }
    }
    mounted
}

/// Does this reopen answer belong on screen NOW, in front of the user
/// who asked for it?
///
/// Only a failure, and only while the asker is still on THAT agent's
/// page: an outcome that arrives after they moved on is history, and
/// a success needs no interruption — the pane it made is the answer.
fn reopen_overlay_for(
    label: &str,
    outcome: &zellij::ReopenOutcome,
    mode: Mode,
    snap: &StatusSnapshot,
) -> bool {
    use zellij::ReopenOutcome as O;
    if matches!(outcome, O::Reopened | O::AlreadyOpen) {
        return false;
    }
    let Mode::AgentDetail { idx, .. } = mode else {
        return false;
    };
    snap.agents.get(idx).is_some_and(|a| a.label == label)
}

fn roster_confirm_after_decision(
    action: ConfirmAction,
    roster_write_succeeded: bool,
    picker_len: usize,
    // Two different numbers since waits became selectable: where the
    // "+ add" row sits, and how many agents there are to index into.
    add_row: usize,
    agents_len: usize,
) -> (Mode, bool) {
    if roster_write_succeeded {
        return (Mode::AgentPanel { sel: add_row }, true);
    }
    match action {
        // A refused add drops back to the step that asked WHAT they
        // review, with the coverage the operator had already chosen.
        ConfirmAction::AddCandidate { idx, tier } if idx < picker_len => (
            Mode::AddReviews {
                candidate: idx,
                kind: tier,
                sel: 0,
            },
            false,
        ),
        ConfirmAction::RemoveAgent { idx } if idx < agents_len => {
            (Mode::AgentDetail { idx, sel: 0 }, false)
        }
        _ => (Mode::AgentPanel { sel: add_row }, true),
    }
}

fn rebind_picker_sel_by_label(
    old_sel: usize,
    label: Option<&str>,
    picker: &[crate::cli::status::AvailableAgent],
) -> Option<usize> {
    if let Some(label) = label {
        return picker.iter().position(|c| c.label.as_str() == label);
    }
    if picker.is_empty() {
        None
    } else {
        Some(old_sel.min(picker.len() - 1))
    }
}

/// Swap `out` for the candidate at `sel`: ONE config write, then back
/// to the panel.
///
/// No pane work, deliberately. The status TUI owns roster->pane
/// convergence (tui-zellij-pane-reconcile), so a pane opened here
/// would make this a second owner — and one opened before the write
/// is roster-absent, which is exactly what the reconciler destroys.
/// The write is the whole action; the watcher observes it and the
/// panes follow.
///
/// A failure keeps the user on the page with the reason, matching
/// `PromoteToMaster`: a silently unchanged roster reads as "clank
/// ignored me".
fn apply_swap(
    out: usize,
    sel: usize,
    snapshot: &mut crate::cli::status::StatusSnapshot,
    picker: &[crate::cli::status::AvailableAgent],
    repo: &std::path::Path,
    home: Option<&std::path::Path>,
    error: &mut Option<(String, String)>,
) -> Mode {
    let (Some(agent), Some(cand)) = (snapshot.agents.get(out), picker.get(sel)) else {
        return Mode::AgentPanel { sel: 0 };
    };
    let (Ok(out_label), Ok(in_label)) = (
        clank_core::ids::AgentLabel::parse(&agent.label),
        clank_core::ids::AgentLabel::parse(&cand.label),
    ) else {
        return Mode::AgentDetail { idx: out, sel: 0 };
    };
    match crate::cli::agent::swap_repo_agent(repo, home, &out_label, &in_label) {
        Ok(()) => Mode::AgentPanel { sel: 0 },
        Err(e) => {
            *error = Some(("swap failed".to_string(), format!("{e:?}")));
            Mode::AgentDetail { idx: out, sel: 0 }
        }
    }
}

/// Where this mode's candidate sits in the picker list, if it holds
/// one.
///
/// Every such mode holds it BY INDEX, and a refresh can reorder the
/// list under it, so the label is captured from here before the
/// refresh and the index re-located after. It is ONE function because
/// it was two: the step that asks what a reviewer reviews was added to
/// the rebind and not to the capture, so an ordinary background
/// refresh dropped the list and sent the operator back to the panel
/// mid-choice (one-menu-not-five).
fn candidate_index(mode: Mode) -> Option<usize> {
    match mode {
        Mode::AddPicker { sel } => Some(sel),
        Mode::SwapPicker { sel, .. } => Some(sel),
        Mode::AddReviews { candidate, .. } => Some(candidate),
        Mode::Confirm {
            action: ConfirmAction::AddCandidate { idx, .. },
        } => Some(idx),
        _ => None,
    }
}

fn rebind_add_picker_after_refresh(
    old_sel: usize,
    label: Option<&str>,
    picker: &[crate::cli::status::AvailableAgent],
    add_row: usize,
) -> Mode {
    match rebind_picker_sel_by_label(old_sel, label, picker) {
        Some(sel) => Mode::AddPicker { sel },
        None => Mode::AgentPanel { sel: add_row },
    }
}

fn rebind_add_confirm_after_refresh(
    old_idx: usize,
    label: Option<&str>,
    picker: &[crate::cli::status::AvailableAgent],
    add_row: usize,
    tier: crate::cli::teams_config::ReviewKind,
) -> Mode {
    let rebound = if let Some(label) = label {
        picker.iter().position(|c| c.label.as_str() == label)
    } else {
        (old_idx < picker.len()).then_some(old_idx)
    };
    match rebound {
        Some(idx) => Mode::Confirm {
            action: ConfirmAction::AddCandidate { idx, tier },
        },
        None => Mode::AgentPanel { sel: add_row },
    }
}

fn rebind_remove_confirm_after_refresh(
    label: Option<&str>,
    agents: &[crate::cli::status::AgentAutoRow],
    add_row: usize,
) -> Mode {
    match label.and_then(|l| relocate_detail(l, agents)) {
        Some(idx) => Mode::Confirm {
            action: ConfirmAction::RemoveAgent { idx },
        },
        None => Mode::AgentPanel { sel: add_row },
    }
}

// `strip_leading_emoji` lives in `text` (it's a name/width helper); the
// the TUI loop's doc moved down onto `run_tui` where it belongs.

/// The log viewport's scroll state, bundled so the invariants that used
/// to live in loose-variable comments are enforced by methods:
/// - the viewport (`lift`, `offset`) is HELD between frames and moved
///   only by [`settle`](LogView::settle), which keeps the cursor on
///   screen, and by [`follow`](LogView::follow), which carries it over a
///   refresh; crossing in from the panel goes through
///   [`enter_first`](LogView::enter_first), which resets all three;
/// - `fill` (permission to do the top-up IO) is REQUESTED by input/data/
///   resize events and consumed once per fill — an animation tick never
///   requests it, so a tick stays a pure repaint.
///
/// The cursor moves are methods so the loop reads as intent
/// (`log.page_down(page)`) and the saturation lives in one place.
#[derive(Clone)]
struct LogView {
    /// Selected entry — index into the scroll sequence.
    cursor: usize,
    /// Viewport top.
    offset: usize,
    /// Header rows lifted off the top for the log.
    lift: usize,
    /// Log fetch-window size; grows on demand until `complete`.
    window: usize,
    /// Cursor for the next older fixed-size page.
    next: Option<crate::cli::log::HistoryCursor>,
    /// The fetch reached the root — stop growing.
    complete: bool,
    /// This pass may do the top-up IO. Set by input/data/resize, cleared
    /// after one fill; a tick never sets it.
    fill: bool,
    /// The cursor last moved up: a rule it comes to rest on is passed
    /// over in that direction.
    rising: bool,
}

impl LogView {
    fn new() -> Self {
        Self {
            cursor: 0,
            offset: 0,
            lift: 0,
            window: 30,
            next: None,
            complete: false,
            fill: true,
            rising: false,
        }
    }

    /// Permit a top-up fill on this pass (input, data change, resize).
    fn request_fill(&mut self) {
        self.fill = true;
    }

    /// Settle the viewport for the paint, moving it no more than keeping
    /// the cursor on screen needs. A focused log at its top is composed
    /// with the header: `lift_for(n)` lifts enough of it for the `n`
    /// entries above the cursor, so the header goes as the cursor
    /// descends and returns as it climbs. Scrolled, the log holds its
    /// lift, taking more of the header before it scrolls further. The
    /// unfocused log keeps its last page full, and its lift for when
    /// focus comes back from a page.
    fn settle(
        &mut self,
        total: usize,
        focused: bool,
        lift_for: impl Fn(usize) -> usize,
        capacity_at: impl Fn(usize) -> usize,
        unselectable: impl Fn(usize) -> bool,
    ) {
        if !focused {
            self.offset = self.offset.min(total.saturating_sub(capacity_at(0).max(1)));
            return;
        }
        self.cursor = land(
            self.cursor.min(total.saturating_sub(1)),
            total,
            self.rising,
            unselectable,
        );
        self.offset = self.offset.min(self.cursor);
        self.lift = if self.offset == 0 {
            lift_for(self.cursor)
        } else {
            self.lift.max(lift_for(self.cursor - self.offset))
        };
        self.offset = scroll_to_show(self.cursor, self.offset, capacity_at(self.lift), total);
    }

    // Each move lands at once, before the next key in the same batch or an
    // Enter can see the cursor: a rule is never a position, however the
    // keys arrive.
    fn moved(&mut self, cursor: usize, rising: bool, total: usize, rule: &dyn Fn(usize) -> bool) {
        self.rising = rising;
        self.cursor = land(cursor, total, rising, rule);
    }
    fn up(&mut self, total: usize, rule: &dyn Fn(usize) -> bool) {
        self.moved(self.cursor.saturating_sub(1), true, total, rule);
    }
    fn down(&mut self, total: usize, rule: &dyn Fn(usize) -> bool) {
        self.moved(self.cursor + 1, false, total, rule);
    }
    fn page_up(&mut self, page: usize, total: usize, rule: &dyn Fn(usize) -> bool) {
        self.moved(self.cursor.saturating_sub(page), true, total, rule);
    }
    fn page_down(&mut self, page: usize, total: usize, rule: &dyn Fn(usize) -> bool) {
        self.moved(self.cursor + page, false, total, rule);
    }
    fn jump_top(&mut self, total: usize, rule: &dyn Fn(usize) -> bool) {
        self.moved(0, true, total, rule);
    }
    fn jump_bottom(&mut self, total: usize, rule: &dyn Fn(usize) -> bool) {
        self.moved(total.saturating_sub(1), false, total, rule);
    }
    /// Keep the selected entry where it sat on screen after a refresh
    /// moved it to `now`: the rows that arrived above it go above the
    /// viewport, and the header keeps its lift.
    fn follow(&mut self, now: usize) {
        self.offset = (self.offset + now).saturating_sub(self.cursor);
        self.cursor = now;
    }
    /// Enter the log at its first entry (crossing in from the panel) —
    /// the one place offset is reset directly, alongside the cursor.
    fn enter_first(&mut self) {
        self.cursor = 0;
        self.offset = 0;
        self.lift = 0;
    }
}

/// The view to paint, with `log` settled into exactly the capacity the
/// render will draw (see [`LogView::settle`]). `head` counts the ask
/// lines before the log rows, and the history must already be topped
/// up, so the cursor clamps here as the paint will.
fn settle_frame<'a>(
    log: &mut LogView,
    snapshot: &StatusSnapshot,
    view: PanelView<'a>,
    rows: u16,
    cols: u16,
    frame: usize,
    head: usize,
) -> PanelView<'a> {
    let log_focused = view.mode.log_focused();
    let total = head + snapshot.log_rows.len();
    // From the TRUE header length: a clipped render reports capacity 0
    // however deep the header overflows (codex be2b054).
    let header_len = render::scrollable_header(snapshot, rows, cols, frame, &view).len();
    log.settle(
        total,
        log_focused,
        |above| pressure_lift(log_focused, rows as usize, header_len, above, total),
        |lift| {
            log_budget(
                rows as usize,
                header_len,
                lift,
                !snapshot.agents.is_empty(),
                log_focused,
                total,
            )
            .capacity
        },
        |i| is_rule(snapshot, head, i),
    );
    PanelView {
        lift: log.lift,
        log_cursor: log.cursor,
        ..view
    }
}

/// The log entry a refresh must keep selected, including under a page
/// opened from it.
fn held_entry(
    mode: Mode,
    snapshot: &StatusSnapshot,
    cols: u16,
    log: &LogView,
) -> Option<scroll::HeldEntry> {
    if mode.panel_tops_log() {
        return None;
    }
    let head = block_ask_spans(snapshot, cols as usize).len();
    scroll::hold_entry(snapshot, head, log.cursor)
}

/// Put the cursor back on `held` in the refreshed `snapshot`, on the
/// screen row it was on. An entry the refresh dropped leaves the cursor
/// where it was.
fn refind_entry(
    log: &mut LogView,
    held: Option<scroll::HeldEntry>,
    snapshot: &StatusSnapshot,
    cols: u16,
) {
    let Some(held) = held else { return };
    let head = block_ask_spans(snapshot, cols as usize).len();
    if let Some(now) = scroll::find_entry(snapshot, head, &held) {
        log.follow(now);
    }
}

/// Where `Up` at the log's top goes: onto the panel row drawn directly
/// above the log, whichever region that is.
fn log_up_crossing(log: &LogView, snapshot: &StatusSnapshot, cols: usize) -> Option<Mode> {
    crosses_into_panel(log.cursor, log.offset, snapshot.agents.len()).then(|| Mode::AgentPanel {
        sel: panel_row_list(snapshot, cols).len().saturating_sub(1),
    })
}

/// A nudge row's page: what happened and the text that was submitted,
/// to read; it has nothing to act on.
fn nudge_detail(row: &crate::cli::log::OnelineRow) -> Option<Overlay> {
    let crate::cli::log::OnelineRow::Nudge {
        agent,
        summary,
        text,
        ..
    } = row
    else {
        return None;
    };
    Some(Overlay {
        data: OverlayData::Typed {
            title: format!("nudge · @{agent}"),
            summary: summary.clone(),
            text: text.clone(),
        },
        offset: 0,
        copied: None,
    })
}

/// Whether scroll entry `i` is a run's end rule, the log's rows starting
/// after `head` ask lines.
fn is_rule(snapshot: &StatusSnapshot, head: usize, i: usize) -> bool {
    i.checked_sub(head).is_some_and(|i| {
        matches!(
            snapshot.log_rows.get(i),
            Some(crate::cli::log::OnelineRow::RunEnd)
        )
    })
}

/// THE INVARIANT: panel focus ⟹ the log viewport is at its top. The
/// panel sits ABOVE the log in one continuous column (Up from the log's
/// first entry crosses into it; Down past the panel's last row crosses
/// back), so a focused panel over a mid-scroll log reads as two cursors
/// at once. Enforced HERE on every loop pass — not by per-transition
/// discipline — so no panel entry path (the Up-crossing, Tab's teleport
/// from any depth, a refresh rebind, or any future one) can violate it.
/// Idempotent. Pages the panel opens find the log already topped, and
/// a page opened from a log entry returns to the viewport it left.
fn enforce_panel_tops_log(mode: Mode, log: &mut LogView) {
    if mode.panel_tops_log() {
        log.enter_first();
    }
}

fn initial_mode(snapshot: &StatusSnapshot) -> Mode {
    if snapshot.log_rows.is_empty() && snapshot.blocks.is_empty() && !snapshot.agents.is_empty() {
        Mode::AgentPanel { sel: 0 }
    } else {
        Mode::LogScroll
    }
}

/// What a full-window document overlay is showing. None re-fetches on a
/// background `Refresh`: each is a moment or a fixed text, and objects
/// that change under the reader are pages (every-object-in-the-tui-has-a-page).
enum OverlayData {
    /// Escaped argv retained by Arc from the accepted observation. Rendering
    /// and scrolling visit only visible bytes, even for large commands.
    Command(std::sync::Arc<wait_detail::Command>),
    /// A failed action's error text (no re-fetch identity — the error
    /// is a moment, not a live document; Refresh leaves it as-is).
    Error { title: String, message: String },
    /// An agent's read-only facts, full and scrollable.
    ///
    /// They do not live on the agent page because an invocation is a
    /// user-configured command line with no bound relative to the pane:
    /// pinned there it pushed the session warning off a short pane, and
    /// windowed there it competed with the menu for the same rows. A
    /// document that scrolls is the shape that always fits, and `y`
    /// copies the command out of it (one-menu-not-five).
    Info { title: String, body: String },
    /// Text that was typed into a terminal, kept as it was laid out,
    /// under one line saying what became of it.
    Typed {
        title: String,
        summary: String,
        text: String,
    },
    /// A link to follow from another device, with its QR
    /// (the-tui-mints-the-way-in).
    Link {
        title: String,
        url: String,
        qr: Vec<String>,
    },
}

/// An open document overlay: its [`OverlayData`] and the scroll
/// `offset`. `Some` ⟺ an overlay is showing; the page beneath keeps its
/// cursor, restored on dismiss.
struct Overlay {
    data: OverlayData,
    offset: usize,
    /// When the text was last copied, so the hint can say so and then
    /// stop saying so. The terminal never answers an OSC 52, so this
    /// is the only acknowledgement there is.
    copied: Option<std::time::Instant>,
}

/// What `y` puts on the clipboard: the retained reason, whole.
///
/// The RETAINED one, not the rendered window — what is painted is
/// wrapped to the region and clipped to its height, and a clipped
/// reason is exactly the one you cannot act on. Only a failure
/// carries text worth taking somewhere else.
fn copyable(data: &OverlayData) -> Option<&str> {
    match data {
        OverlayData::Error { message, .. } => Some(message),
        // An invocation is a command line to paste somewhere else,
        // which is the same need the failure overlay had.
        OverlayData::Info { body, .. } => Some(body),
        OverlayData::Typed { text, .. } => Some(text),
        _ => None,
    }
}

/// How long the overlay says `copied` before going back to `y copy`.
const COPIED_FOR: Duration = Duration::from_secs(2);

/// How long an overlay with nothing to say waits for an event.
const OVERLAY_IDLE: Duration = Duration::from_secs(60);

/// Whether the overlay is still saying `copied`, and for how much
/// longer.
///
/// One value decides both what is painted and how long the loop then
/// sleeps. Deciding them separately lets them disagree across the
/// two-second boundary: the screen says `copied` while the sleep
/// concludes there is nothing to come back for, and the word stays up
/// for a minute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hint {
    Copied(Duration),
    Idle,
}

impl Hint {
    fn saying_copied(self) -> bool {
        matches!(self, Hint::Copied(_))
    }
    /// How long the loop may sleep on this hint: just long enough to
    /// come back and unsay it, or the idle backstop.
    fn wait(self) -> Duration {
        match self {
            Hint::Copied(left) => left,
            Hint::Idle => OVERLAY_IDLE,
        }
    }
}

/// The hint as of ONE reading of the clock.
///
/// `now` is passed in rather than read here so the caller samples
/// once, and so the boundary itself can be tested. Every subtraction
/// is saturating or checked: crossing the boundary between two reads
/// of the clock used to underflow `Duration` and take the whole TUI
/// down over a copy.
fn hint_at(copied: Option<std::time::Instant>, now: std::time::Instant) -> Hint {
    let Some(at) = copied else {
        return Hint::Idle;
    };
    match COPIED_FOR.checked_sub(now.saturating_duration_since(at)) {
        Some(left) if !left.is_zero() => Hint::Copied(left),
        _ => Hint::Idle,
    }
}

impl Overlay {
    /// An agent's facts as a scrollable document.
    fn info(title: String, body: String) -> Self {
        Self {
            data: OverlayData::Info { title, body },
            offset: 0,
            copied: None,
        }
    }
    /// A failed plan-page action's error, opened at the top
    /// (errors-in-the-TUI, tui-plan-actions-page).
    fn error(title: String, message: String) -> Self {
        Self {
            data: OverlayData::Error { title, message },
            offset: 0,
            copied: None,
        }
    }
    /// The error doc is the TUI's one titled page; a notice — the
    /// remote switch saying where it listens — is that page with a
    /// calmer title.
    fn notice(n: remote::Notice) -> Self {
        Self::error(n.title, n.message)
    }
    fn link(title: String, url: String) -> Self {
        Self {
            data: OverlayData::Link {
                title,
                qr: remote::qr_lines(&url),
                url,
            },
            offset: 0,
            copied: None,
        }
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
    // Stats degrade to empty on failure — the page still opens
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

/// The subject's one plan tag, when its page can be read. Invalid names
/// remain untrusted input and never become filesystem paths.
fn commit_plan(
    repo: &std::path::Path,
    subject: &str,
    snapshot: &StatusSnapshot,
) -> Option<crate::lifecycle::PlanKey> {
    let clank_core::repo_state::TitlePrefix::Plan(name) =
        clank_core::repo_state::parse_title_prefix(subject)?;
    let key = crate::lifecycle::PlanKey::parse(&name).ok()?;
    has_plan_page(repo, key.as_str(), snapshot).then_some(key)
}

/// A commit's page, opened on its rows — or, from a review row, with
/// the document focused at `focus`'s feedback. `None` when the commit
/// cannot be read.
fn open_commit_page(
    repo: &std::path::Path,
    sha: &crate::lifecycle::CommitSha,
    focus: Option<&str>,
    snapshot: &StatusSnapshot,
    cols: usize,
) -> Option<(CommitPage, Mode)> {
    let detail = fetch_commit_detail(repo, sha)?;
    let plan = commit_plan(repo, &detail.subject, snapshot);
    let (sel, scroll) = match focus {
        Some(author) => (
            document_focus(&commit_actions(&plan)),
            commit_review_offset(&commit_doc(&detail), author, cols),
        ),
        None => (0, 0),
    };
    Some((
        CommitPage {
            detail,
            plan,
            scroll,
            left_at: 0,
        },
        Mode::CommitPage { sel },
    ))
}

/// Re-read the open commit page by its sha, keeping the reader's
/// scroll, and carry the cursor by the row under it. A commit that
/// cannot be read now keeps what the page showed.
fn refresh_commit_page(
    repo: &std::path::Path,
    cp: &mut CommitPage,
    sel: usize,
    snapshot: &StatusSnapshot,
) -> Mode {
    let before = commit_actions(&cp.plan);
    if let Some(detail) = fetch_commit_detail(repo, &cp.detail.sha) {
        cp.detail = detail;
    }
    cp.plan = commit_plan(repo, &cp.detail.subject, snapshot);
    Mode::CommitPage {
        sel: rebind_page_sel(&before, sel, &commit_actions(&cp.plan)),
    }
}

/// Close the plan page, returning to the drafts list row it was opened
/// from, or the commit page `beneath` it on the row it was left on; else
/// to the log.
fn close_plan_page(
    repo: &std::path::Path,
    plan_page: &mut Option<PlanPage>,
    beneath: &mut Option<CommitPage>,
    commit_page: &mut Option<CommitPage>,
    snapshot: &StatusSnapshot,
) -> Mode {
    if let Some(row) = plan_page.take().and_then(|pp| pp.drafts_row) {
        return Mode::DraftsList {
            sel: row.min(snapshot.drafts.len().saturating_sub(1)),
        };
    }
    let Some(mut cp) = beneath.take() else {
        return Mode::LogScroll;
    };
    let left_at = cp.left_at;
    let mode = refresh_commit_page(repo, &mut cp, left_at, snapshot);
    *commit_page = Some(cp);
    mode
}

/// Refresh the open plan page, or close it if its plan is gone; `Err`
/// is where closing it landed.
async fn refetch_or_close(
    repo: &std::path::Path,
    plan_page: &mut Option<PlanPage>,
    beneath: &mut Option<CommitPage>,
    commit_page: &mut Option<CommitPage>,
    snapshot: &StatusSnapshot,
) -> Result<(), Mode> {
    match refetch_plan_page(repo, plan_page, snapshot).await {
        true => Ok(()),
        false => Err(close_plan_page(
            repo,
            plan_page,
            beneath,
            commit_page,
            snapshot,
        )),
    }
}

/// An active or finished plan's page, from the plans.
async fn open_plan_page(
    repo: &std::path::Path,
    key: &crate::lifecycle::PlanKey,
    snapshot: &StatusSnapshot,
) -> Option<PlanPage> {
    let stem = key.as_str();
    let (st, _) = plan_page_facts(repo, stem, snapshot, PlanSource::Plans).await?;
    Some(PlanPage {
        stem: stem.to_string(),
        st,
        entry: None,
        body: read_plan_markdown(repo, stem),
        scroll: 0,
        drafts_row: None,
    })
}

/// The page of the plan on commit row `key`. The commit page moves
/// `beneath` it, remembering that row for the plan page's Back — the
/// row fired, not the cursor, which a digit key leaves elsewhere.
async fn open_plan_from_commit(
    repo: &std::path::Path,
    commit_page: &mut Option<CommitPage>,
    beneath: &mut Option<CommitPage>,
    key: &crate::lifecycle::PlanKey,
    snapshot: &StatusSnapshot,
) -> Option<PlanPage> {
    let pp = open_plan_page(repo, key, snapshot).await?;
    let mut cp = commit_page.take()?;
    cp.left_at = commit_actions(&cp.plan)
        .iter()
        .position(|a| matches!(a, CommitAction::OpenPlan(k) if k == key))
        .unwrap_or(0);
    *beneath = Some(cp);
    Some(pp)
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

/// What a page can open as HTML — a plan stem, a full commit sha, a
/// queue entry's file stem, a stash stem.
enum HtmlTarget {
    Plan(String),
    Commit(String),
    Queue(String),
    Stash(String),
}

/// Open a page's object as its rendered HTML page in the host browser.
/// Spawns the built `clank` DETACHED with stdio nulled: the TUI
/// owns the terminal (raw/alt-screen), so build progress must not print into
/// the pane, and detaching lets the browser open when the incremental build
/// finishes without freezing the loop. For a commit whose page was evicted
/// from the incremental window, pass `--rebuild` so the open still succeeds.
fn open_in_browser(repo: &std::path::Path, target: &HtmlTarget) {
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

const REOPEN_NOTICE: &str = "reopen pane";

/// What the gate is waiting on this agent for, with the TUI's verb
/// and since when. The clock is what the page's `auto` compares to
/// find whoever has been kept waiting longest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Owed {
    pub(crate) verb: String,
    /// The sha, plan, PR or block the routing names — what the verb
    /// is about.
    pub(crate) object: String,
    pub(crate) since: Option<i64>,
}

/// One agent: master first, then the reviewers in roster order.
/// `owes` is both what this agent is doing and the fact that it is
/// this agent's turn — there is no separate list of holders to
/// disagree with. `last` is the agent's newest act in the log.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Position {
    pub(crate) label: String,
    /// The harness whose terminal control keys apply to this agent.
    pub(crate) tool: String,
    /// The configured launch command shown on the agent detail page.
    pub(crate) invocation: String,
    /// The bound session scopes messages waiting for transcript proof.
    pub(crate) session: Option<String>,
    pub(crate) role: String,
    /// Effective auto mode and its per-agent override, if one exists.
    pub(crate) auto: clank_core::vocab::AutoMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) auto_explicit: Option<clank_core::vocab::AutoMode>,
    pub(crate) owes: Option<Owed>,
    /// The question the agent is waiting on you to answer: what it asks.
    pub(crate) asking: Option<String>,
    pub(crate) last: Option<String>,
    /// Whether clank can show this agent's transcript, or only its
    /// terminal.
    pub(crate) transcript: bool,
    /// Why Chat cannot open when there is no transcript to follow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) chat_unavailable_reason: Option<String>,
}

/// One row of the ledger: the TUI's log row as data, in the TUI's
/// order, with the same age.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct LedgerRow {
    pub(crate) kind: String,
    pub(crate) glyph: Option<String>,
    pub(crate) sha: Option<String>,
    pub(crate) text: String,
    pub(crate) author: Option<String>,
    pub(crate) verdict: Option<String>,
    /// The class the page colours the mark by, for a review row.
    ///
    /// Separate from `verdict`, which is the word a reader sees: a
    /// colour taken from prose is a colour that breaks when the prose
    /// is reworded, and "requested changes" is not one class.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) verdict_class: Option<String>,
    /// The plan this commit row stands for, when it is that plan's
    /// only commit. Carried SEPARATELY from `text` so the page can
    /// give the name the emphasis the header it replaced had — text
    /// alone left it reading as an ordinary subject (codex on
    /// f99d42a).
    pub(crate) stands_for: Option<String>,
    pub(crate) age: Option<String>,
    /// The row's page in the `clank html` site, relative to its root
    /// (`commit/<sha>.html`, `plan/<stem>.html`); a review's is its
    /// commit's. `None` for the ad-hoc umbrella and github rows.
    pub(crate) page: Option<String>,
}

/// One queued plan, as the page draws it and names it back.
///
/// The HANDLE is the file's stem, not the parsed name: `queue add`
/// allows two files that parse to one name, so a page that said
/// "promote foo" would promote whichever was found first
/// (a-queued-plan-has-a-page).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Queued {
    pub(crate) name: String,
    pub(crate) priority: u16,
    pub(crate) handle: String,
    pub(crate) page: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Stashed {
    pub(crate) stem: String,
    pub(crate) waiting_for: Option<String>,
    pub(crate) ready: bool,
    pub(crate) commits: usize,
    pub(crate) page: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct PendingBlock {
    pub(crate) agent: String,
    pub(crate) name: String,
    /// Returned with an answer, so it lands only on the ask shown.
    pub(crate) occurrence: Option<String>,
    pub(crate) question: String,
}

/// What has been done, and what the repo is carrying.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Ledger {
    pub(crate) plan: Option<String>,
    pub(crate) gate: Option<String>,
    pub(crate) dirty: Option<String>,
    /// What is queued, in the order it will run. The count the page
    /// shows is these entries' length: a number beside the list is a
    /// second thing to keep true (the-queue-is-workable-from-the-phone).
    pub(crate) queue: Vec<Queued>,
    /// How many plan ideas wait in `.clank/drafts/`: the panel's one
    /// collapsed row, said the same way here. Nothing acts on them
    /// remotely.
    pub(crate) drafts: usize,
    pub(crate) stash: usize,
    pub(crate) stash_entries: Vec<Stashed>,
    pub(crate) blocks: Vec<PendingBlock>,
    pub(crate) rows: Vec<LedgerRow>,
}

/// The TUI's derived facts for the web page. Derived HERE, inside
/// the module whose `pub(super)` items know how — the page must say
/// what the pane says, and the only way to guarantee that is to ask
/// the same functions.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct WebFacts {
    /// The repo this page is about, named as `clank status` names it.
    /// A second opinion about a repo's name would be worse than none,
    /// so this is the snapshot's own basename and nothing else.
    pub(crate) project: String,
    /// The lamp's text and its plan, as `bar_text` gives them.
    pub(crate) lamp: String,
    pub(crate) plan: String,
    /// The frame's hue as hex, from the same table as the bar.
    pub(crate) hue: String,
    /// HEAD's tag needs fixing, said as `clank status` says it.
    /// Master's to repair, but not an obligation the gate routes, so
    /// it belongs to the session and not to an agent.
    pub(crate) correction: Option<String>,
    pub(crate) agents: Vec<Position>,
    pub(crate) ledger: Ledger,
}

/// `with_transcript`: the agents whose transcript the server is
/// following — the page's knowledge, not the snapshot's.
pub(crate) fn web_facts(
    snap: &StatusSnapshot,
    with_transcript: &std::collections::BTreeSet<String>,
) -> WebFacts {
    use clank_core::vocab::Role;
    let (lamp, plan) = render::bar_text(snap);
    let hue = derive::state_color(snap).hex();
    let work = derive::work_projection(snap);

    // What each agent owes is the TUI's spinning row; the verb and the
    // object come from the same routing that made it spin.
    let mut owed: std::collections::HashMap<String, Owed> = in_progress_rows(snap)
        .into_iter()
        .map(|p| {
            let (label, verb, since, role) = match p {
                InProgress::PendingReview { label, verb, since } => {
                    (label, verb, since, Role::Reviewer)
                }
                InProgress::MasterWorking { name, verb, since } => {
                    (name, verb, since, Role::Master)
                }
            };
            let object = clank_core::ids::AgentLabel::parse(&label)
                .ok()
                .and_then(|l| work.work_for(&l, role).into_iter().next())
                .map(|item| object_of(&item))
                .unwrap_or_default();
            (
                label,
                Owed {
                    verb: verb.to_string(),
                    object,
                    since,
                },
            )
        })
        .collect();

    // Master first, then the roster's order.
    let mut ordered: Vec<&crate::cli::status::AgentAutoRow> = Vec::new();
    if let Some(m) = snap
        .agents
        .iter()
        .find(|a| a.role == crate::cli::teams_config::RosterRole::Master)
    {
        ordered.push(m);
    }
    ordered.extend(
        snap.agents
            .iter()
            .filter(|a| a.role != crate::cli::teams_config::RosterRole::Master),
    );
    let agents = ordered
        .into_iter()
        .map(|a| Position {
            label: a.label.clone(),
            tool: a.tool.clone(),
            invocation: a.invocation.clone(),
            session: a.session.as_ref().map(|s| s.id.as_str().to_string()),
            role: render::tier_label(a.role).to_string(),
            auto: a.auto_mode,
            auto_explicit: a.auto_mode_explicit,
            owes: owed.remove(&a.label),
            asking: a.asking.clone(),
            last: last_act(snap, &a.label, a.role),
            transcript: with_transcript.contains(&a.label),
            chat_unavailable_reason: None,
        })
        .collect();

    let ledger = Ledger {
        plan: snap
            .active_plan
            .as_ref()
            .map(|p| p.plan.as_str().to_string()),
        gate: snap
            .active_plan
            .as_ref()
            .map(|p| gate_word(p.gate).to_string()),
        dirty: snap.dirty.as_ref().map(crate::cli::status::dirty_summary),
        queue: snap
            .queue
            .iter()
            .map(|q| {
                let handle = q
                    .path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Queued {
                    name: q.name.clone(),
                    priority: q.priority,
                    page: crate::cli::html::queue_page_for_stem(&handle),
                    handle,
                }
            })
            .collect(),
        drafts: snap.drafts.len(),
        stash: snap.stash.len(),
        stash_entries: snap
            .stash
            .iter()
            .map(|s| Stashed {
                stem: s.stem.clone(),
                waiting_for: s.waiting_for.clone(),
                ready: s.ready,
                commits: s.commits,
                page: crate::cli::html::stash_page_for_stem(&s.stem),
            })
            .collect(),
        blocks: snap
            .blocks
            .iter()
            .filter(|b| b.answer.is_none())
            .map(|b| PendingBlock {
                agent: b.agent.clone(),
                name: b.name.clone(),
                occurrence: b.occurrence.clone(),
                question: b.question.clone(),
            })
            .collect(),
        rows: ledger_rows(&snap.log_rows),
    };

    WebFacts {
        project: snap.basename.clone(),
        lamp,
        plan,
        hue,
        correction: snap.head_correction.as_ref().map(|c| {
            format!(
                "HEAD {} — amend the tag ({})",
                c.sha.as_str(),
                crate::cli::status::describe_head_violation(&c.violation)
            )
        }),
        agents,
        ledger,
    }
}

/// What a routed item is about, as the floor says it.
fn object_of(item: &clank_core::wait::WaitItem) -> String {
    use clank_core::wait::WaitItem;
    let short = |s: &clank_core::ids::CommitSha| s.as_str()[..7.min(s.as_str().len())].to_string();
    match item {
        WaitItem::Master { plan, .. } => plan.as_str().to_string(),
        WaitItem::Reviewer { sha, .. } => short(sha),
        WaitItem::AdHocReview { sha, .. } | WaitItem::AdHocRevise { sha } => {
            format!("{} (ad-hoc)", short(sha))
        }
        _ => String::new(),
    }
}

/// The agent's newest act in the log: a reviewer's newest verdict,
/// the master's newest commit.
fn last_act(
    snap: &StatusSnapshot,
    label: &str,
    role: crate::cli::teams_config::RosterRole,
) -> Option<String> {
    use crate::cli::log::OnelineRow;
    if role == crate::cli::teams_config::RosterRole::Master {
        let row = snap
            .log_rows
            .iter()
            .find(|r| matches!(r, OnelineRow::Commit { .. }))?;
        let OnelineRow::Commit { marker, .. } = row else {
            return None;
        };
        return Some(match render::row_age(row) {
            Some(age) => format!("{} committed, {age} ago", marker.glyph()),
            None => format!("{} committed", marker.glyph()),
        });
    }
    let row = snap
        .log_rows
        .iter()
        .find(|r| matches!(r, OnelineRow::Review { author, .. } if author == label))?;
    let OnelineRow::Review { verdict, .. } = row else {
        return None;
    };
    let mark = crate::cli::log::verdict_mark(*verdict, false);
    Some(match render::row_age(row) {
        Some(age) => format!("{mark} {}, {age} ago", verdict_word(*verdict)),
        None => format!("{mark} {}", verdict_word(*verdict)),
    })
}

/// The CSS class the remote page colours a review mark by.
///
/// Derived here rather than from `verdict_word` in the page's script,
/// because the words are prose and one of them has a SPACE in it:
/// interpolating "requested changes" into a class attribute makes two
/// classes, neither of which any rule names (codex 90780fa). These
/// tokens are the wire contract between the row and the stylesheet.
pub(crate) fn verdict_class(v: clank_core::vocab::Verdict) -> &'static str {
    use clank_core::vocab::Verdict;
    match v {
        Verdict::Continue => "v-continue",
        Verdict::Finished => "v-finished",
        Verdict::RequestChanges => "v-changes",
        Verdict::Unmarked => "v-unmarked",
    }
}

fn verdict_word(v: clank_core::vocab::Verdict) -> &'static str {
    use clank_core::vocab::Verdict;
    match v {
        Verdict::Continue => "continued",
        Verdict::Finished => "finished",
        Verdict::RequestChanges => "requested changes",
        Verdict::Unmarked => "unmarked",
    }
}

fn gate_word(g: clank_core::vocab::CommitGateState) -> &'static str {
    use clank_core::vocab::CommitGateState as G;
    match g {
        G::Unreviewed => "unreviewed",
        G::Continued => "continued",
        G::ContinuedPendingGate => "awaiting the gate",
        G::ChangesRequested => "changes requested",
        G::Finished => "finished",
        G::Blocked => "blocked",
    }
}

/// The log's rows for the page. The log is newest first, and a
/// review sits ABOVE the commit it reviewed (`log.rs`, the review
/// rows) naming no sha of its own — so its page is the next commit's
/// BELOW it, found by walking up from the bottom, and never one from
/// past an umbrella, which is a group of its own. Measured live
/// before it was believed: the first cut carried the commit above.
fn ledger_rows(rows: &[crate::cli::log::OnelineRow]) -> Vec<LedgerRow> {
    use crate::cli::log::OnelineRow;
    let mut commit_page: Option<String> = None;
    let mut out: Vec<LedgerRow> = rows
        .iter()
        .rev()
        .map(|r| {
            let mut row = ledger_row(r);
            match r {
                OnelineRow::Commit { .. } | OnelineRow::PlainCommit { .. } => {
                    commit_page = row.page.clone();
                }
                OnelineRow::Review { .. } => row.page = commit_page.clone(),
                OnelineRow::Header { .. } => commit_page = None,
                OnelineRow::Github { .. }
                | OnelineRow::Notice(_)
                | OnelineRow::RunEnd
                | OnelineRow::Nudge { .. } => {}
            }
            row
        })
        .collect();
    out.reverse();
    out
}

fn ledger_row(row: &crate::cli::log::OnelineRow) -> LedgerRow {
    use crate::cli::log::OnelineRow;
    let age = render::row_age(row);
    let short = |s: &crate::lifecycle::CommitSha| s.as_str()[..7.min(s.as_str().len())].to_string();
    let commit_page = |s: &crate::lifecycle::CommitSha| Some(format!("commit/{}.html", s.as_str()));
    match row {
        OnelineRow::Header {
            plan,
            duration_secs,
            ..
        } => LedgerRow {
            kind: "header".into(),
            glyph: None,
            sha: None,
            text: crate::cli::log::plan_footer_text(plan.as_deref(), *duration_secs),
            author: None,
            verdict: None,
            verdict_class: None,
            stands_for: None,
            age,
            page: plan.as_ref().map(|p| format!("plan/{p}.html")),
        },
        OnelineRow::Commit {
            sha,
            subject,
            marker,
            stands_for,
            ..
        } => LedgerRow {
            // Still `commit`: the page keeps its own kind, its own
            // sha and its own link, and a review above it still
            // attributes to it. Only the words change — the plan's
            // name where its finish message would have been.
            kind: "commit".into(),
            glyph: Some(marker.glyph().to_string()),
            sha: Some(short(sha)),
            text: stands_for.clone().unwrap_or_else(|| subject.clone()),
            stands_for: stands_for.clone(),
            author: None,
            verdict: None,
            verdict_class: None,
            age,
            page: commit_page(sha),
        },
        // Pre-adoption history: the site never builds this page, so
        // the server renders it on request from git (codex on b1c0cf8).
        OnelineRow::PlainCommit { sha, subject, .. } => LedgerRow {
            kind: "commit".into(),
            glyph: None,
            sha: Some(short(sha)),
            text: subject.clone(),
            author: None,
            verdict: None,
            verdict_class: None,
            stands_for: None,
            age,
            page: commit_page(sha),
        },
        OnelineRow::Review {
            verdict,
            author,
            summary,
            ..
        } => LedgerRow {
            kind: "review".into(),
            glyph: Some(crate::cli::log::verdict_mark(*verdict, false)),
            sha: None,
            text: summary.clone(),
            author: Some(author.clone()),
            verdict: Some(verdict_word(*verdict).into()),
            verdict_class: Some(verdict_class(*verdict).into()),
            stands_for: None,
            age,
            page: None,
        },
        OnelineRow::Github {
            line,
            backend,
            glyph,
            delivery,
            ..
        } => LedgerRow {
            kind: "github".into(),
            glyph: Some(format!(
                "{} {}",
                delivery.glyph(),
                crate::cli::log::badge_of(glyph.as_deref(), *backend)
            )),
            sha: None,
            text: line.clone(),
            author: None,
            verdict: None,
            verdict_class: None,
            stands_for: None,
            age,
            page: None,
        },
        OnelineRow::RunEnd => LedgerRow {
            kind: "run_end".into(),
            glyph: None,
            sha: None,
            text: String::new(),
            author: None,
            verdict: None,
            verdict_class: None,
            stands_for: None,
            age: None,
            page: None,
        },
        OnelineRow::Notice(n) => LedgerRow {
            kind: "notice".into(),
            glyph: None,
            sha: None,
            text: n.clone(),
            author: None,
            verdict: None,
            verdict_class: None,
            stands_for: None,
            age,
            page: None,
        },
        OnelineRow::Nudge { agent, summary, .. } => LedgerRow {
            kind: "nudge".into(),
            glyph: Some(crate::cli::log::NUDGE_GLYPH.into()),
            sha: None,
            text: summary.clone(),
            author: Some(agent.clone()),
            verdict: None,
            verdict_class: None,
            stands_for: None,
            age,
            page: None,
        },
    }
}

/// The spinner's frame rate — the only thing on screen that needs a
/// fast tick.
const SPINNER_TICK: Duration = Duration::from_millis(500);

/// How long the pane sleeps with nothing animating. Every elapsed
/// clank draws is minute-resolution ([`crate::age::coarse`]), so this
/// is what keeps them true: a repaint at least as often as the
/// smallest unit shown, and no timer of their own.
const IDLE_TICK: Duration = Duration::from_secs(60);

/// The repaint cadence. Ages on screen never raise it — they are
/// recomputed by the paint that was going to happen anyway.
fn paint_interval(spinner_visible: bool) -> Duration {
    if spinner_visible {
        SPINNER_TICK
    } else {
        IDLE_TICK
    }
}

/// The panel's own probe for a waker that died.
///
/// Every other snapshot input announces itself: a watcher event fires
/// because a file changed. A waiter being killed changes NO file, so
/// nothing raises `refresh_pending` and the loop redraws the snapshot
/// it already holds — the panel kept showing a healthy row after the
/// waiter died, including across key repaints (codex 451c5dbc,
/// grok 8aa2736120d7). The signature being ABLE to see it is not the
/// same as the loop ever asking.
struct AttendanceProbe {
    at: std::time::Instant,
    fingerprint: u64,
}

/// Ask, at most once per `every`, whether the verdict changed.
///
/// `true` means "raise `refresh_pending`" — the nothing-changed gate
/// still decides whether a rebuild follows, so a flip costs one
/// signature check rather than an unconditional refold.
///
/// Rate-limited around the probe, not inside it: a key storm turns
/// this loop many times, and the reads are cheap but not free.
fn attendance_changed(
    state: &mut AttendanceProbe,
    now: std::time::Instant,
    every: Duration,
    probe: impl FnOnce() -> u64,
) -> bool {
    if now.duration_since(state.at) < every {
        return false;
    }
    state.at = now;
    let fingerprint = probe();
    let changed = fingerprint != state.fingerprint;
    state.fingerprint = fingerprint;
    changed
}

/// What a turn asks for once its input has been processed.
///
/// Returned rather than applied, so a test can see what the turn
/// DECIDED. The attendance check used to sit inline in the loop with
/// its answer assigned straight to a local, which left the decision
/// invisible from outside: the helper had six assertions and its call
/// site had none, and deleting the call changed no test's verdict
/// (codex on fbb84e3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TurnClose {
    /// Whether a snapshot rebuild is owed after this turn.
    refresh_pending: bool,
    /// How many times the fingerprint was actually read. Zero when the
    /// rate limit was not due, which is the property that keeps a key
    /// storm from turning this into real work.
    observed: usize,
}

/// Close a turn: the one input with no event behind it.
///
/// Runs on EVERY turn — timeout, key, resize and data wake alike —
/// because a waiter being killed changes no file, so nothing else
/// raises `refresh_pending` and the loop would repaint the snapshot it
/// already holds. That is the bug this exists for: the panel showed a
/// healthy row after the waiter had died, including across key
/// repaints.
///
/// `refresh_pending` goes in and comes back so a rebuild already owed
/// from this turn's own input cannot be lost here, and so the probe is
/// visibly NOT conditional on whether one was.
fn close_turn(
    probe: &mut AttendanceProbe,
    now: std::time::Instant,
    every: Duration,
    refresh_pending: bool,
    observe: impl FnOnce() -> u64,
) -> TurnClose {
    let mut observed = 0usize;
    let changed = attendance_changed(probe, now, every, || {
        observed += 1;
        observe()
    });
    TurnClose {
        refresh_pending: refresh_pending || changed,
        observed,
    }
}

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

/// Backoff before RETRYING a failed rebuild
/// (tui-transient-refresh-resilience, codex 889c637): 1s, 2s, 4s …
/// capped at 30s — a lone transient failure recovers in about a
/// second without needing a filesystem event, while a persistent
/// one polls gently instead of spinning the rebuild loop hot.
/// Event-driven recovery is unaffected (a real refresh event still
/// rebuilds immediately). Pure → unit-tested.
fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs(1u64 << failures.saturating_sub(1).min(5)).min(Duration::from_secs(30))
}

/// A drained burst of loop events collapsed for ONE pass: keystrokes in
/// arrival order, and ANY number of `Refresh`/`Resize` folded to a single
/// flag each. So a storm of watcher wakes costs one repaint + at most one
/// (throttled) rebuild, not one per event.
struct Batch {
    input: Vec<u8>,
    refresh: bool,
    resize: bool,
    /// Body fetches that landed this beat, in arrival order.
    content: Vec<(
        u64,
        clank_core::wait::ContentRef,
        event_content::ContentState,
    )>,
    process: Vec<wait_process::Completion>,
}

/// What one event arriving BEHIND an overlay owes the page beneath.
///
/// The overlay answers the event itself — scrolling on a key — and
/// then `continue`s
/// past the loop bottom where the main page's refresh flush and log
/// fill live. So the flags those need are raised from here, or never:
/// a refresh consumed by an overlay used to leave the main page
/// painting the snapshot from before the overlay opened until the
/// NEXT watcher wake, which on a quiet repo is a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct BehindOverlay {
    /// Flag the coalesced snapshot rebuild for the first pass after
    /// the overlay closes.
    refresh: bool,
    /// Top up the log viewport, as a resize on the main page would.
    fill: bool,
}

fn behind_overlay(ev: &Ev) -> BehindOverlay {
    match ev {
        Ev::Refresh => BehindOverlay {
            refresh: true,
            fill: false,
        },
        Ev::Resize => BehindOverlay {
            refresh: false,
            fill: true,
        },
        Ev::Stdin(_) | Ev::Content { .. } | Ev::Process(_) | Ev::Worker | Ev::Remote => {
            BehindOverlay::default()
        }
    }
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
        content: Vec::new(),
        process: Vec::new(),
    };
    for ev in events {
        match ev {
            Ev::Stdin(bytes) => b.input.extend(bytes),
            Ev::Refresh => b.refresh = true,
            Ev::Resize => b.resize = true,
            // Each completion is DATA, not a flag — folding two into
            // one would drop an answer the page is waiting for.
            Ev::Content {
                generation,
                target,
                state,
            } => b.content.push((generation, target, state)),
            Ev::Process(completion) => b.process.push(completion),
            Ev::Worker | Ev::Remote => {}
        }
    }
    b
}

/// The TUI loop, fully event-driven: the watcher covers the
/// working tree (gitignore-filtered), `.clank/`, and the git dir;
/// SIGWINCH arrives on the same channel, so a resize is just
/// another wake. Between events there is nothing to redraw —
/// nothing rendered is clock-relative — so the only timeout is a
/// slow backstop against watcher pathologies the error channel
/// doesn't surface (tui-event-driven-dirty-stats).
/// `clank tui`: resolve the repo and run the loop.
pub async fn run(args: crate::cli::TuiArgs) -> anyhow::Result<()> {
    let repo = crate::cli::resolve_repo(args.repo.as_deref())?;
    let basename = crate::cli::repo_basename(&repo)?;
    let policy = if args.no_cache {
        crate::rebuild::CachePolicy::Bypass
    } else {
        crate::rebuild::CachePolicy::Use
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    run_tui(repo, basename, home, policy).await
}

pub(crate) async fn run_tui(
    repo: PathBuf,
    basename: String,
    home: Option<PathBuf>,
    policy: crate::rebuild::CachePolicy,
) -> anyhow::Result<()> {
    // Ownership first, before a watcher, a worker or the alt screen:
    // a second TUI for this repo must print where the first one is and
    // leave the terminal exactly as it found it.
    let _lease = lease::acquire(&repo).map_err(|why| anyhow::anyhow!("{why}"))?;

    let (tx, rx) = mpsc::channel::<()>();
    let _watcher = watch_status_paths(tx, &repo)?;
    // Resize is NOT a data change, so SIGWINCH gets its OWN channel and
    // maps to `Ev::Resize` — otherwise the nothing-changed gate (which
    // only fires on `Ev::Refresh`) would swallow it and leave a taller
    // pane underfilled until the next keypress.
    let (winch_tx, winch_rx) = mpsc::channel::<()>();
    spawn_sigwinch_forwarder(winch_tx)?;

    // Merge the watcher (data), SIGWINCH (resize), stdin (keys), and
    // the zellij worker's answers into one event stream the loop
    // drains. A blocking `read` on stdin IS the notification (no
    // polling).
    let (ev_tx, ev_rx) = mpsc::channel::<Ev>();
    let (web_reopen_tx, web_reopen_rx) =
        mpsc::channel::<(String, tokio::sync::oneshot::Sender<Result<String, String>>)>();
    let web_reopen: crate::cli::web::Reopener = {
        let wake = ev_tx.clone();
        std::sync::Arc::new(move |label| {
            let (reply, answer) = tokio::sync::oneshot::channel();
            if web_reopen_tx.send((label, reply)).is_ok() {
                let _ = wake.send(Ev::Remote);
            }
            answer
        })
    };

    // Declared BEFORE the alt-screen guard: reverse drop order joins
    // the worker AFTER the terminal is restored, on every exit path
    // (tui-reconcile-off-loop).
    // `starting` is published before the watch that serves `clank
    // remote` exists, so a caller waits for `ready` instead of concluding
    // anything from a missing status.
    let mut served = remote_owner::Served::starting(&repo);
    let mut remote_notice = None;
    let (remote_req_tx, remote_req_rx) = mpsc::channel::<String>();
    let runtime = {
        let wake = ev_tx.clone();
        zellij::RuntimeWatch::start(&repo, remote_req_tx, move || {
            let _ = wake.send(Ev::Remote);
        })
    };
    let mut reconcile_worker = {
        let wake = ev_tx.clone();
        zellij::ReconcileWorker::spawn(repo.clone(), &runtime, move || {
            let _ = wake.send(Ev::Worker);
        })
    };
    // Declared here for the same reason: its drop ends the server
    // after the terminal is back, so a slow exit is seen in a shell.
    let mut remote = {
        let wake = ev_tx.clone();
        let zellij_repo = repo.clone();
        remote::Remote::new(
            repo.clone(),
            home.clone(),
            std::sync::Arc::new(move || {
                Ok(
                    std::sync::Arc::new(crate::cli::web::Zellij::find(&zellij_repo)?)
                        as std::sync::Arc<dyn crate::cli::web::Panes>,
                )
            }),
            crate::cli::web::PANE_POLL,
            crate::cli::web::STOP_GRACE,
            remote::Browser,
            std::sync::Arc::new(move || {
                let _ = wake.send(Ev::Remote);
            }),
        )
        .reopen_through(web_reopen)
        .tunnel_from({
            let home = home.clone();
            std::sync::Arc::new(move || crate::cli::web::tunnel::configured(home.as_deref()))
        })
    };
    served.armed(runtime.unavailable.clone(), &remote);

    let _guard = AltScreen::enter();
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
    // Kept for body fetches and process observations; the stdin reader
    // takes its own clone rather than the original.
    let ev_tx_async = ev_tx.clone();
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

    // Probe the input signature BEFORE the first build so any change
    // racing the build re-builds next wake (under-gate, never over-gate).
    let mut last_sig = crate::cli::status::input_signature(&repo).ok();
    // Consecutive failed refreshes (tui-transient-refresh-resilience);
    // reset by every successful rebuild. `refresh_retry_at` schedules
    // the bounded self-retry after a failure.
    let mut refresh_failures: u32 = 0;
    let mut refresh_retry_at: Option<std::time::Instant> = None;
    let mut snapshot =
        StatusSnapshot::build_async(&repo, &basename, home.as_deref(), policy, None, true).await?;
    // Roster→pane convergence is the worker's job; this is a channel
    // send, not zellij work (tui-reconcile-off-loop).
    reconcile_worker.observe(&snapshot);
    reconcile_worker.update_glyphs(&snapshot);
    // The log viewport — cursor, derived offset, and the on-demand fetch
    // window. Older history is appended in fixed-size pages from `next`;
    // already loaded commits are not reread merely because the cursor moved.
    // Keep at least `offset + viewport` rows loaded, which both fills a tall
    // pane on first paint and pages in older rows as you scroll. See
    // [`LogView`] for the invariants its methods enforce.
    let mut log = LogView::new();
    log.next = snapshot.log_next.clone();
    log.complete = log.next.is_none();
    // The document overlay, when open: an agent's facts, a failure, a
    // link.
    let mut detail: Option<Overlay> = None;
    let mut detail_shown: Option<Vec<DetailAction>> = None;
    // Which region owns the keyboard. Starts on the log; Tab moves it to
    // the agent panel. The single source of key-routing truth.
    let mut mode = initial_mode(&snapshot);
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
    // The source page's source (⟺ mode is `SourcePage`), the page an
    // event page returns to when opened from it, and the configuration
    // a restart confirm was shown, which the monitor checks it still runs.
    let mut source_page: Option<input::SourceRef> = None;
    let mut source_group: Option<input::GroupRef> = None;
    let mut event_beneath: Option<Mode> = None;
    let mut restart_accepted: Option<String> = None;
    // The open commit page, and the one a plan page opened from it
    // returns to; see `Mode::CommitPage`.
    let mut commit_page: Option<CommitPage> = None;
    let mut commit_beneath: Option<CommitPage> = None;
    // The open EVENT page (tui-github-event-page); invariant: `Some`
    // ⟺ mode is `EventDetail` (set/cleared together, like plan_page).
    let mut event_page: Option<EventPage> = None;
    let mut wait_page: Option<crate::cli::status_tui::input::WaitPage> = None;
    let mut wait_process = wait_process::Controller::default();
    let mut wait_scroll: usize = 0;
    let mut seen_session: Option<crate::cli::status_tui::input::SeenSession> = None;
    let mut clear_target: Option<crate::cli::status_tui::input::ClearTarget> = None;
    // Monotonic across the whole session: every request the page makes
    // gets a number, and only the newest one's answer is wanted
    // (tui-github-event-content, Contract 3).
    let mut content_generation: u64 = 0;
    // The open plan-page text input's buffer (Mode::PlanInput carries
    // only the KIND — same Copy-preserving split as `plan_page`).
    let mut plan_input: Option<TextInput> = None;
    // INVARIANT: `Some` ⟺ mode is `BlockPage`.
    let mut block_page: Option<block_page::BlockPage> = None;
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
    // Explicit reopen outcomes are returned in request order. A TUI
    // keypress owns a slot too, so a simultaneous web request cannot
    // mistake the keypress's answer for its own.
    let mut reopen_pending: std::collections::VecDeque<(
        String,
        Option<tokio::sync::oneshot::Sender<Result<String, String>>>,
    )> = Default::default();
    let mut reopen_outcomes_pending = Vec::new();
    let mut waker_probe = AttendanceProbe {
        at: std::time::Instant::now(),
        fingerprint: crate::cli::status::attendance_fingerprint(&repo),
    };
    let mut admin_page_cache: Option<RemotePage> = None;
    let mut admin_page_shown = None;
    let mut admin_page_error: Option<String> = None;
    let mut admin_refresh = false;
    // Labeled so a `Quit` key inside the per-key drain loop exits the
    // event loop, not just the inner `for`.
    'evloop: loop {
        reconcile_wait_process(
            &mut mode,
            &mut wait_page,
            &snapshot.agents,
            &mut wait_process,
        );
        pump_wait_process(&mut wait_process, &ev_tx_async);
        for (label, answer) in web_reopen_rx.try_iter() {
            reopen_pending.push_back((label.clone(), Some(answer)));
            reconcile_worker.reopen(&label);
        }
        if let Some(n) = remote_owner::pump(&mut served, &mut remote, &remote_req_rx, &snapshot)
            .await
            .pop()
        {
            remote_notice = Some(n);
        }
        // Answer web requests even while a document overlay owns the TUI.
        // Keep the outcomes for the normal panel path to paint afterward.
        let reopen_outcomes = reconcile_worker.outcomes();
        for (label, outcome) in &reopen_outcomes {
            if let Some(index) = reopen_pending.iter().position(|(name, _)| name == label)
                && let Some((_, Some(answer))) = reopen_pending.remove(index)
            {
                let message = outcome.describe(label);
                let _ = answer.send(if *outcome == zellij::ReopenOutcome::Reopened {
                    Ok(message)
                } else {
                    Err(message)
                });
            }
        }
        reopen_outcomes_pending.extend(reopen_outcomes);
        let (rows, cols) = term_size();
        // Every pass, before any render: see the fn's invariant doc.
        enforce_panel_tops_log(mode, &mut log);

        // A document overlay owns the whole pane until dismissed:
        // render it, route its own (scroll / back) keys, and skip the
        // normal log/panel path. Nothing here animates, so the wait is
        // the slow backstop.
        if detail.is_some() {
            let page = (rows as usize).saturating_sub(3).max(1);
            let overlay = detail.as_ref().unwrap();
            // Sampled once: the hint that gets painted and the wait
            // that must come back to unpaint it are one decision.
            let hint = hint_at(overlay.copied, std::time::Instant::now());
            let (lines, total) = match &overlay.data {
                OverlayData::Command(command) => (
                    render_wait_command(command, overlay.offset, rows as usize, cols as usize),
                    0,
                ),
                OverlayData::Info { title, body } => {
                    render_info_doc(title, body, overlay.offset, rows as usize, cols as usize)
                }
                OverlayData::Typed {
                    title,
                    summary,
                    text,
                } => render_typed_doc(
                    title,
                    summary,
                    text,
                    overlay.offset,
                    rows as usize,
                    cols as usize,
                ),
                OverlayData::Error { title, message } => render_error_doc(
                    title,
                    message,
                    overlay.offset,
                    rows as usize,
                    cols as usize,
                    hint.saying_copied(),
                ),
                OverlayData::Link { title, url, qr } => {
                    render_link_doc(title, url, qr, overlay.offset, rows as usize, cols as usize)
                }
            };
            paint(&lines);
            let ev = ev_rx.recv_timeout(hint.wait());
            // The page beneath is hidden, not gone: what a wake means
            // for it is decided once, here, and carried out of this
            // loop — the flush at the loop bottom is never reached
            // while an overlay is open, so a flag not raised here is a
            // wake lost for good (an-overlay-does-not-eat-the-refresh).
            if let Ok(ev) = &ev {
                let step = behind_overlay(ev);
                if step.refresh {
                    refresh_pending = true;
                }
                if step.fill {
                    log.request_fill();
                }
            }
            match ev {
                // The page is behind this overlay, not gone: applying
                // here keeps a completion that arrives mid-overlay
                // instead of stranding the slot on Loading.
                Ok(Ev::Content {
                    generation,
                    target,
                    state,
                }) => {
                    let before = event_page.as_ref().map(actions_for).unwrap_or_default();
                    if let Some(slot) = event_page.as_mut().and_then(|p| p.content.as_mut()) {
                        slot.apply(generation, &target, state);
                    }
                    // The page is behind this overlay and its stored
                    // selection outlives it, so rebind here too.
                    if let (Some(ep), Mode::EventDetail { sel }) = (event_page.as_ref(), &mode) {
                        let after = actions_for(ep);
                        mode = Mode::EventDetail {
                            sel: rebind_event_sel(&before, *sel, &after),
                        };
                    }
                }
                Ok(Ev::Stdin(bytes)) => {
                    for k in parse_keys(&bytes) {
                        if let Some(d) = detail.as_mut()
                            && let OverlayData::Command(command) = &d.data
                        {
                            match k {
                                Key::Top => {
                                    d.offset = 0;
                                    continue;
                                }
                                Key::Bottom => {
                                    d.offset = command.tail(
                                        (rows as usize).saturating_sub(1).max(1),
                                        cols as usize,
                                    );
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        match doc_nav(k, page) {
                            DocNav::Back => detail = None,
                            DocNav::Scroll(delta) => {
                                let max_off = total.saturating_sub((rows as usize).max(1));
                                if let Some(d) = detail.as_mut() {
                                    if let OverlayData::Command(command) = &d.data {
                                        d.offset = command.advance(d.offset, delta, cols as usize);
                                    } else {
                                        d.scroll(delta, max_off);
                                    }
                                }
                            }
                            DocNav::Copy => {
                                // Only the error overlay carries a
                                // reason worth taking somewhere else.
                                if let Some(d) = detail.as_mut()
                                    && let Some(text) = copyable(&d.data)
                                {
                                    super::term::copy(text);
                                    d.copied = Some(std::time::Instant::now());
                                }
                            }
                            DocNav::None => {}
                        }
                        if detail.is_none() {
                            break; // dismissed — remaining keys route next pass
                        }
                    }
                }
                Ok(Ev::Process(completion)) => {
                    wait_process.complete(completion);
                }
                // The overlay hides the notice row; the answer is
                // drained and painted once the overlay closes. A refresh
                // and a resize were noted above.
                Ok(Ev::Refresh) | Ok(Ev::Resize) | Ok(Ev::Worker) | Ok(Ev::Remote) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("event channel disconnected")
                }
            }
            continue;
        }

        // Presence may have moved an agent page's action list since
        // the last frame; the cursor follows its action before this
        // one paints.
        // The remote's word is for the operator wherever they are:
        // the overlay mounts over any page, and the loop goes round
        // to draw it, as a reopen's does.
        if let Some(n) = remote_notice.take() {
            detail = Some(Overlay::notice(n));
            continue;
        }
        let presence = reconcile_worker.presence();
        mode = settle_detail_cursor(mode, &snapshot.agents, &presence, &mut detail_shown);
        crate::cli::status_tui::input::settle_seen_session(
            mode,
            &snapshot.agents,
            &mut seen_session,
            |tool, id| crate::session_probe::session_jsonl_exists(&tool, id, home.as_deref()),
        );
        if !matches!(
            mode,
            Mode::Confirm {
                action: ConfirmAction::ClearSession { .. }
            }
        ) {
            clear_target = None;
        }
        let shown = remote.shown();
        // Keep the host's answer while one of its pages is open. A
        // repaint can happen every 120ms; only entering a page, a
        // state change, or a mutation needs another network request.
        if reads_server_admin(mode) {
            if admin_page_cache.is_none() || admin_page_shown != Some(shown) || admin_refresh {
                admin_page_shown = Some(shown);
                admin_refresh = false;
                admin_page_error = None;
                let mut fetch_failed = false;
                admin_page_cache = Some(match remote.admin_state().await {
                    Ok(Some(state)) => RemotePage {
                        token: state.token,
                        sessions: state.sessions,
                        admin_available: true,
                    },
                    Ok(None) => RemotePage {
                        admin_available: false,
                        ..Default::default()
                    },
                    Err(error) => {
                        fetch_failed = true;
                        let why = format!("{error:#}");
                        admin_page_error = Some(why.clone());
                        detail = Some(Overlay::error("server controls unavailable".into(), why));
                        RemotePage {
                            admin_available: false,
                            ..Default::default()
                        }
                    }
                });
                if fetch_failed {
                    continue;
                }
            }
        } else {
            admin_page_cache = None;
            admin_page_shown = None;
            admin_page_error = None;
        }
        let remote_page = admin_page_cache.as_ref();
        let remote_detail = match (remote.detail(), admin_page_error.as_deref()) {
            (Some(detail), Some(error)) => Some(format!("{detail} · server controls: {error}")),
            (None, Some(error)) => Some(format!("server controls: {error}")),
            (detail, None) => detail,
        };
        let view = PanelView {
            source_page: source_page.as_ref(),
            source_group: source_group.as_ref(),
            wait_page: wait_page.as_ref(),
            wait_process: wait_process.slot.as_ref(),
            wait_scroll,
            seen_session: seen_session.as_ref(),
            clear_target: clear_target.as_ref(),
            mode,
            plan_page: plan_page.as_ref(),
            commit_page: commit_page.as_ref(),
            event_page: event_page.as_ref(),
            plan_input: plan_input.as_ref(),
            block_page: block_page.as_ref(),
            picker: &picker,
            log_cursor: log.cursor,
            lift: 0,
            // A non-blocking drain: the worker reports after each
            // batch, the loop never waits on zellij to paint.
            reach: reconcile_worker.reach(),
            remote: shown,
            remote_detail: remote_detail.as_deref(),
            remote_page,
            presence,
            lost: reconcile_worker.lost(),
        };
        // An overlay mounted here is drawn by the branch at the TOP of
        // the loop, which this pass is already past: without going
        // round again the refusal waits for the next event, and with
        // nothing animating that is the 60-second idle tick (codex on
        // 6303167).
        if mount_reopen_outcomes(
            std::mem::take(&mut reopen_outcomes_pending),
            mode,
            &mut snapshot,
            &mut detail,
        ) {
            continue;
        }
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
        // cursor can move past the loaded tail). Runs BEFORE the lift is
        // derived, so the policy sees the real post-fill total (an
        // unclamped cursor or an empty sequence must not over-lift —
        // codex ce616ce). The viewport can never exceed the pane, so
        // `rows` bounds the fill regardless of the eventual lift. Gated
        // on `log.fill` so an animation tick never reaches it.
        if log.fill {
            let want = (log.offset + rows as usize).max(log.cursor + 1);
            while !log.complete && head + snapshot.log_rows.len() < want {
                let page_size = (rows as usize).max(1);
                let Some(tip) = log.next.clone() else {
                    log.complete = true;
                    break;
                };
                let page = crate::cli::status::tui_log_page(
                    &repo,
                    &tip,
                    page_size,
                    snapshot.log_adopted_at.as_ref(),
                    &snapshot.log_stands_for,
                )
                .await;
                log.window += page_size;
                log.next = page.next.clone();
                snapshot.log_next = page.next;
                crate::cli::log::extend_oneline_rows(&mut snapshot.log_rows, page.rows);
                log.complete = log.next.is_none();
            }
            log.fill = false;
        }

        let total = head + snapshot.log_rows.len();
        if total == 0 && matches!(mode, Mode::LogScroll) && !snapshot.agents.is_empty() {
            // An unborn repository has no timeline entry to select. Keep
            // focus on the real panel instead of presenting a cursor in an
            // empty region.
            mode = Mode::AgentPanel { sel: 0 };
            continue;
        }
        let view = settle_frame(&mut log, &snapshot, view, rows, cols, frame, head);
        // `screen_total` is the current screen's virtual content height
        // (log rows; plan page chrome + full document) — the plan page's
        // body-scroll clamp reads it, mirroring the overlay pattern.
        let (screen, screen_total) = render_at(&snapshot, rows, cols, log.offset, frame, &view);
        paint(&screen);

        // The spinner lives on the AGENTS panel rows, which are always on
        // screen when a roster exists — tick whenever any agent is active
        // (no window math; the log carries no placeholders).
        let spinner_visible = !in_prog.is_empty() && !snapshot.agents.is_empty();
        let base = paint_interval(spinner_visible);
        // A scheduled retry that has come due is promoted to a normal
        // deferred refresh; a pending one caps the sleep so the
        // deadline actually fires (a plain Timeout wake loops back
        // here and promotes).
        if let Some(at) = refresh_retry_at
            && at <= std::time::Instant::now()
        {
            refresh_pending = true;
            refresh_retry_at = None;
        }
        let wait = deferred_wait(base, refresh_pending, last_rebuild.elapsed(), REBUILD_MIN);
        let wait = match refresh_retry_at {
            Some(at) => wait.min(at.saturating_duration_since(std::time::Instant::now())),
            None => wait,
        };
        let wait = match until_next_due(&snapshot, crate::age::now()) {
            Some(due) => wait.min(due),
            None => wait,
        };

        match ev_rx.recv_timeout(wait) {
            // Drain the whole queued burst in ONE pass: keystrokes apply
            // in arrival order, and any number of Refresh/Resize fold to a
            // single flag (coalesce), so a watcher storm costs one repaint
            // — not one loop turn per event.
            Ok(first) => {
                let batch = coalesce(
                    std::iter::once(first).chain(std::iter::from_fn(|| ev_rx.try_recv().ok())),
                );
                for completion in batch.process {
                    wait_process.complete(completion);
                }
                // Body fetches that landed: each is applied only if
                // the page still wants it (Contract 3). A refused one
                // is silently dropped — it answers a question nobody
                // is asking any more.
                if !batch.content.is_empty() {
                    let before = event_page.as_ref().map(actions_for).unwrap_or_default();
                    for (generation, target, state) in batch.content {
                        if let Some(slot) = event_page.as_mut().and_then(|p| p.content.as_mut()) {
                            slot.apply(generation, &target, state);
                        }
                    }
                    // Availability may have moved under the cursor;
                    // keep it on the action it was on.
                    if let (Some(ep), Mode::EventDetail { sel }) = (event_page.as_ref(), &mode) {
                        let after = actions_for(ep);
                        mode = Mode::EventDetail {
                            sel: rebind_event_sel(&before, *sel, &after),
                        };
                    }
                }
                let was_on_agents = mode_shows_agents(&mode);
                // Incremental parse: ONE key at a time, interpretation
                // picked from the CURRENT mode — a pasted `f<subject>⏎`
                // crosses into input mode mid-buffer and its text must
                // not be pre-parsed as commands.
                let mut inbuf = batch.input.as_slice();
                loop {
                    let text_mode = match mode {
                        Mode::PlanInput { .. } | Mode::PauseInput => true,
                        Mode::BlockPage => block_page.as_ref().is_some_and(|p| p.typing),
                        _ => false,
                    };
                    let Some((anykey, used)) = parse_one(inbuf, text_mode) else {
                        if inbuf.is_empty() {
                            break;
                        }
                        inbuf = &inbuf[1..];
                        continue;
                    };
                    inbuf = &inbuf[used..];
                    // The page owns both its buttons and its input, so
                    // every key goes to it whichever way it parsed.
                    if let Mode::BlockPage = mode {
                        match drive_block_page(
                            &mut block_page,
                            anykey,
                            &snapshot,
                            rows as usize,
                            cols as usize,
                            |ask, text| block_page::answer_in_store(&repo, ask, text),
                        ) {
                            PageStep::To(m) => mode = m,
                            PageStep::Answered(m) => {
                                mode = m;
                                refresh_pending = true;
                            }
                            PageStep::Quit => break 'evloop,
                        }
                        continue;
                    }
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
                            } else if matches!(mode, Mode::PauseInput) {
                                let outcome = plan_input
                                    .as_mut()
                                    .map(|ti| text_input_nav(ti, tk))
                                    .unwrap_or(InputNav::Cancel);
                                // Pause is repo state, so every exit
                                // from this input returns to the log —
                                // there may be no plan page to go back to.
                                match outcome {
                                    InputNav::Cancel => {
                                        plan_input = None;
                                        mode = Mode::LogScroll;
                                    }
                                    InputNav::Submit => {
                                        let buf = plan_input
                                            .as_ref()
                                            .map(|ti| ti.buf.clone())
                                            .unwrap_or_default();
                                        match submit_pause_question(&buf, &snapshot, &repo).await {
                                            InputSubmit::Stay => {}
                                            InputSubmit::Done => {
                                                plan_input = None;
                                                mode = Mode::LogScroll;
                                                refresh_pending = true;
                                            }
                                            InputSubmit::Failed(title, msg) => {
                                                plan_input = None;
                                                detail = Some(Overlay::error(title, msg));
                                                mode = Mode::LogScroll;
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
                        Mode::RemotePage { sel } => {
                            use crate::cli::status_tui::input::{RemoteAction, RemoteNav};
                            // The rows the key was pressed on are the
                            // rows that were PAINTED, not a fresh
                            // read: the host's state may have changed.
                            let actions = crate::cli::status_tui::input::remote_actions_for(
                                remote.shown() == remote::Shown::On,
                                remote_page.is_some_and(|page| page.admin_available),
                            );
                            let sel = sel.min(actions.len().saturating_sub(1));
                            match crate::cli::status_tui::input::remote_page_nav(sel, &actions, k) {
                                RemoteNav::Quit => break 'evloop,
                                RemoteNav::Back => {
                                    let rows = panel_row_list(&snapshot, cols as usize);
                                    mode = Mode::AgentPanel {
                                        sel: rows
                                            .iter()
                                            .position(|r| {
                                                *r == crate::cli::status_tui::input::PanelRow::Remote
                                            })
                                            .unwrap_or(0),
                                    };
                                }
                                RemoteNav::MoveCursor(s) => mode = Mode::RemotePage { sel: s },
                                RemoteNav::Activate(RemoteAction::Switch) => {
                                    if let Some(n) = remote.toggle(&snapshot) {
                                        detail = Some(Overlay::notice(n));
                                    }
                                }
                                RemoteNav::Activate(RemoteAction::Open) => {
                                    remote.open_again().await
                                }
                                RemoteNav::Activate(RemoteAction::Phone) => {
                                    if let Some(url) = remote.link().await {
                                        detail =
                                            Some(Overlay::link("link a phone".to_string(), url));
                                    }
                                }
                                // Both are PAGES now: reading a credential
                                // off a pane and rotating it are two
                                // interactions, and a list of sessions
                                // to manage is a list of objects.
                                RemoteNav::Activate(ref a)
                                    if crate::cli::status_tui::input::remote_destination(a)
                                        .is_some() =>
                                {
                                    mode = crate::cli::status_tui::input::remote_destination(a)
                                        .expect("just checked");
                                }
                                // Every remaining Activate opens a page,
                                // which the guard above took.
                                RemoteNav::Activate(_) | RemoteNav::None => {}
                            }
                            continue;
                        }
                        Mode::SourcePage { sel, lift } => {
                            use crate::cli::status_tui::input::{SourceNav, SourceRow};
                            let Some(source) =
                                source_page.as_ref().and_then(|r| r.find(&snapshot.sources))
                            else {
                                source_page = None;
                                mode = Mode::LogScroll;
                                continue;
                            };
                            let rows_of = source_rows_in(source, &snapshot);
                            match input::source_nav(sel, &rows_of, k) {
                                SourceNav::Quit => break 'evloop,
                                SourceNav::Back => {
                                    mode = source_back(
                                        source,
                                        source_group.as_ref(),
                                        &snapshot,
                                        cols as usize,
                                    );
                                    source_page = None;
                                }
                                nav @ (SourceNav::Sel(_) | SourceNav::Lift(_)) => {
                                    if let Some((sel, lift)) = input::source_moved(sel, lift, nav) {
                                        mode = Mode::SourcePage { sel, lift };
                                    }
                                }
                                SourceNav::Act(SourceRow::Restart) => {
                                    restart_accepted = snapshot
                                        .sources_live
                                        .of(source)
                                        .map(|h| h.accepted.clone());
                                    mode = Mode::Confirm {
                                        action: ConfirmAction::RestartSource,
                                    };
                                }
                                SourceNav::Act(SourceRow::Event(at)) => {
                                    if let Some(page) =
                                        snapshot.github_events.get(at).and_then(|ev| {
                                            open_event_page(
                                                &repo,
                                                ev,
                                                &mut content_generation,
                                                &ev_tx_async,
                                            )
                                        })
                                    {
                                        event_page = Some(page);
                                        event_beneath = Some(Mode::SourcePage { sel, lift });
                                        mode = Mode::EventDetail { sel: 0 };
                                    }
                                }
                                SourceNav::None => {}
                            }
                            continue;
                        }
                        Mode::TimersList { sel } => {
                            use crate::cli::status_tui::input::ListNav;
                            let timers = input::active_timers(&snapshot.sources);
                            match input::list_nav(sel, timers.len(), k) {
                                ListNav::Quit => break 'evloop,
                                ListNav::Back => {
                                    let rows = panel_row_list(&snapshot, cols as usize);
                                    mode = Mode::AgentPanel {
                                        sel: row_position(&rows, input::PanelRow::Timers),
                                    };
                                }
                                ListNav::Sel(x) => mode = Mode::TimersList { sel: x },
                                ListNav::Open(i) => {
                                    source_page = timers.get(i).map(|s| input::SourceRef::of(s));
                                    source_group = None;
                                    mode = Mode::SourcePage { sel: 0, lift: 0 };
                                }
                                ListNav::None => {}
                            }
                            continue;
                        }
                        Mode::SourceMembers { sel } => {
                            use crate::cli::status_tui::input::ListNav;
                            let Some(group) = source_group.clone() else {
                                mode = Mode::LogScroll;
                                continue;
                            };
                            let members = group.members(&snapshot.sources);
                            match input::list_nav(sel, members.len(), k) {
                                ListNav::Quit => break 'evloop,
                                ListNav::Back => {
                                    mode = source_row_in_panel(&group, &snapshot, cols as usize);
                                    source_group = None;
                                }
                                ListNav::Sel(x) => mode = Mode::SourceMembers { sel: x },
                                ListNav::Open(i) => {
                                    source_page = members.get(i).map(|s| input::SourceRef::of(s));
                                    mode = Mode::SourcePage { sel: 0, lift: 0 };
                                }
                                ListNav::None => {}
                            }
                            continue;
                        }
                        Mode::DraftsList { sel } => {
                            use crate::cli::status_tui::input::ListNav;
                            match crate::cli::status_tui::input::list_nav(
                                sel,
                                snapshot.drafts.len(),
                                k,
                            ) {
                                ListNav::Quit => break 'evloop,
                                ListNav::Back => {
                                    mode = Mode::AgentPanel {
                                        sel: row_position(
                                            &panel_row_list(&snapshot, cols as usize),
                                            crate::cli::status_tui::input::PanelRow::Drafts,
                                        ),
                                    };
                                }
                                ListNav::Sel(x) => mode = Mode::DraftsList { sel: x },
                                ListNav::Open(i) => {
                                    if let Some(draft) = snapshot.drafts.get(i) {
                                        plan_page = Some(draft_page(draft, i));
                                        mode = Mode::PlanDetail { sel: 0 };
                                    }
                                }
                                ListNav::None => {}
                            }
                            continue;
                        }
                        Mode::PriorityPage { value } => {
                            use crate::cli::status_tui::input::PriorityNav;
                            match crate::cli::status_tui::input::priority_page_nav(value, k) {
                                PriorityNav::Quit => break 'evloop,
                                // Nothing written: a half-typed number
                                // must not become the plan's priority.
                                PriorityNav::Cancel => mode = Mode::PlanDetail { sel: 0 },
                                PriorityNav::Set(v) => mode = Mode::PriorityPage { value: v },
                                PriorityNav::Commit(v) => {
                                    if let Some(live) = plan_page.as_mut()
                                        && let Err(e) = set_priority_to(&repo, live, v)
                                    {
                                        detail = Some(Overlay::error(
                                            "priority change failed".to_string(),
                                            format!("{e:?}"),
                                        ));
                                    }
                                    mode = Mode::PlanDetail { sel: 0 };
                                    refresh_pending = true;
                                }
                                PriorityNav::None => {}
                            }
                            continue;
                        }
                        Mode::AddReviews {
                            candidate,
                            kind,
                            sel,
                        } => {
                            use crate::cli::status_tui::input::AddReviewNav;
                            match crate::cli::status_tui::input::add_review_nav(sel, k) {
                                AddReviewNav::Quit => break 'evloop,
                                // Nothing is written until Confirm, so
                                // stepping back leaves the roster alone.
                                AddReviewNav::Back => {
                                    mode = Mode::AddPicker { sel: candidate };
                                }
                                AddReviewNav::MoveCursor(s) => {
                                    mode = Mode::AddReviews {
                                        candidate,
                                        kind,
                                        sel: s,
                                    };
                                }
                                AddReviewNav::Toggle(a) => {
                                    // The roster's own rule, so the two
                                    // surfaces cannot disagree about what
                                    // a tick means.
                                    let kind =
                                        crate::cli::status_tui::input::tier_after_toggle(kind, a)
                                            .unwrap_or(kind);
                                    mode = Mode::AddReviews {
                                        candidate,
                                        kind,
                                        sel,
                                    };
                                }
                                AddReviewNav::Confirm => {
                                    mode = Mode::Confirm {
                                        action: ConfirmAction::AddCandidate {
                                            idx: candidate,
                                            tier: kind,
                                        },
                                    };
                                }
                                AddReviewNav::None => {}
                            }
                            continue;
                        }
                        Mode::TokenPage { scroll } => {
                            use crate::cli::status_tui::input::TokenNav;
                            match crate::cli::status_tui::input::token_page_nav(k) {
                                TokenNav::Quit => break 'evloop,
                                TokenNav::Back => mode = Mode::RemotePage { sel: 0 },
                                TokenNav::Scroll(delta) => {
                                    // Clamped against what the last
                                    // render reported, exactly as the
                                    // plan document is.
                                    let max_off =
                                        screen_total.saturating_sub((rows as usize).max(1));
                                    mode = Mode::TokenPage {
                                        scroll: (scroll as i64 + delta as i64)
                                            .clamp(0, max_off as i64)
                                            as usize,
                                    };
                                }
                                TokenNav::Rotate => {
                                    if let Err(e) = remote.rotate_token().await {
                                        detail = Some(Overlay::error(
                                            "token not rotated".to_string(),
                                            format!("{e:#}"),
                                        ));
                                    }
                                    admin_refresh = true;
                                    // A new credential is a new
                                    // document: read it from the top.
                                    mode = Mode::TokenPage { scroll: 0 };
                                }
                                TokenNav::None => {}
                            }
                            continue;
                        }
                        Mode::SessionsPage { sel } => {
                            use crate::cli::status_tui::input::SessionsNav;
                            let page = remote_page.cloned().unwrap_or_default();
                            let count = page.sessions.len();
                            match crate::cli::status_tui::input::sessions_page_nav(sel, count, k) {
                                SessionsNav::Quit => break 'evloop,
                                SessionsNav::Back => mode = Mode::RemotePage { sel: 0 },
                                SessionsNav::MoveCursor(s) => {
                                    mode = Mode::SessionsPage { sel: s };
                                }
                                SessionsNav::Revoke(at) => {
                                    // The rows the key was pressed on are
                                    // the rows that were PAINTED.
                                    if let Some(s) = page.sessions.get(at)
                                        && let Err(e) = remote.revoke_session(&s.id_hash).await
                                    {
                                        detail = Some(Overlay::error(
                                            "session not revoked".to_string(),
                                            format!("{e:#}"),
                                        ));
                                    }
                                    admin_refresh = true;
                                    mode = Mode::SessionsPage {
                                        sel: at.min(count.saturating_sub(2)),
                                    };
                                }
                                SessionsNav::None => {}
                            }
                            continue;
                        }
                        Mode::WaitDetail { agent, sel } => {
                            let att = snapshot
                                .agents
                                .get(agent)
                                .and_then(|a| a.attending.as_ref());
                            let actions = crate::cli::status_tui::input::wait_actions(
                                att.is_some_and(|a| a.killable_pid().is_some()),
                            );
                            match crate::cli::status_tui::input::wait_page_nav(sel, &actions, k) {
                                WaitNav::Quit => break 'evloop,
                                WaitNav::Back => {
                                    wait_page = None;
                                    wait_process.close();
                                    mode = Mode::AgentPanel { sel: 0 };
                                }
                                WaitNav::MoveCursor(s) => mode = Mode::WaitDetail { agent, sel: s },
                                WaitNav::Kill => {
                                    mode = Mode::Confirm {
                                        action: ConfirmAction::KillAttended,
                                    }
                                }
                                WaitNav::Refresh => {
                                    if let Some(page) = wait_page.as_ref() {
                                        wait_process.request(page.clone());
                                    }
                                }
                                WaitNav::FullCommand => {
                                    if let Some(command) = wait_process
                                        .slot
                                        .as_ref()
                                        .and_then(|slot| slot.command.as_ref())
                                    {
                                        detail = Some(Overlay {
                                            data: OverlayData::Command(command.clone()),
                                            offset: 0,
                                            copied: None,
                                        });
                                    } else {
                                        detail = Some(Overlay::info(
                                            "command".into(),
                                            wait_command_unknown(
                                                wait_process.slot.as_ref().map(|slot| &slot.state),
                                            ),
                                        ));
                                    }
                                }
                                WaitNav::Scroll(direction) => {
                                    let page =
                                        wait_page_budget(rows as usize, actions.len()).body.max(1);
                                    let max = screen_total.saturating_sub(page);
                                    wait_scroll = if direction < 0 {
                                        wait_scroll.min(max).saturating_sub(page)
                                    } else {
                                        wait_scroll.saturating_add(page).min(max)
                                    };
                                }
                                WaitNav::Top => wait_scroll = 0,
                                WaitNav::Bottom => {
                                    let page =
                                        wait_page_budget(rows as usize, actions.len()).body.max(1);
                                    wait_scroll = screen_total.saturating_sub(page);
                                }
                                WaitNav::None => {}
                            }
                            continue;
                        }
                        Mode::AgentPanel { sel } => {
                            let panel = panel_row_list(&snapshot, cols as usize);
                            match agent_panel_action(sel, &panel, k) {
                                PanelAction::Quit => break 'evloop,
                                PanelAction::LeaveFocus => {
                                    if total > 0 {
                                        mode = mode.toggle_focus(snapshot.agents.len())
                                    }
                                }
                                PanelAction::MoveCursor(s) => mode = Mode::AgentPanel { sel: s },
                                PanelAction::OpenWait(agent) => {
                                    if let Some((label, att)) =
                                        snapshot.agents.get(agent).and_then(|a| {
                                            a.attending.as_ref().map(|t| (a.label.clone(), t))
                                        })
                                    {
                                        let page = WaitPage::new(&label, att);
                                        wait_scroll = 0;
                                        wait_process.request(page.clone());
                                        wait_page = Some(page);
                                        mode = Mode::WaitDetail { agent, sel: 0 };
                                    }
                                }
                                PanelAction::EnterLog => {
                                    if total > 0 {
                                        // Cross into the log at the FIRST entry.
                                        log.enter_first();
                                        mode = Mode::LogScroll;
                                    }
                                }
                                PanelAction::ToggleAuto(i) => {
                                    if let Some(row) = snapshot.agents.get(i) {
                                        let next = flip_auto(row.auto_mode);
                                        if let Ok(label) =
                                            clank_core::ids::AgentLabel::parse(&row.label)
                                        {
                                            // Single source for the write
                                            // (preserves other fields). On
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
                                                snapshot.agents[i].auto_mode_explicit = Some(next);
                                            }
                                        }
                                    }
                                }
                                PanelAction::ToggleRemote => {
                                    if let Some(n) = remote.toggle(&snapshot) {
                                        detail = Some(Overlay::notice(n));
                                    }
                                }
                                PanelAction::OpenRemote => remote.open_again().await,
                                PanelAction::OpenRemotePage => mode = Mode::RemotePage { sel: 0 },
                                PanelAction::OpenPicker => {
                                    // Read the candidates FRESH right now.
                                    picker = crate::cli::status::available_agents(
                                        home.as_deref(),
                                        &snapshot.agents,
                                    );
                                    mode = Mode::AddPicker { sel: 0 };
                                }
                                PanelAction::OpenDetail(i) => {
                                    (mode, block_page) = agent_row_enter(i, &snapshot);
                                }
                                PanelAction::OpenQueueItem(q) => {
                                    if let Some(item) = snapshot.queue.get(q) {
                                        let name = item.name.clone();
                                        // The row IS the file: its body is
                                        // read from its own path, never
                                        // looked up by name.
                                        plan_page = Some(PlanPage {
                                            body: std::fs::read_to_string(&item.path).ok(),
                                            stem: name,
                                            st: input::PlanPageState::Queued {
                                                priority: item.priority,
                                            },
                                            entry: Some(item.path.clone()),
                                            scroll: 0,
                                            drafts_row: None,
                                        });
                                        mode = Mode::PlanDetail { sel: 0 };
                                    }
                                }
                                PanelAction::OpenStashItem(i) => {
                                    if let Some(item) = snapshot.stash.get(i) {
                                        plan_page = Some(stash_page(&repo, &item.stem));
                                        mode = Mode::PlanDetail { sel: 0 };
                                    }
                                }
                                PanelAction::OpenDrafts => mode = Mode::DraftsList { sel: 0 },
                                PanelAction::OpenTimers => mode = Mode::TimersList { sel: 0 },
                                PanelAction::OpenSource(i) => {
                                    if let Some((page, group, opened)) =
                                        open_source_row(i, &snapshot.sources)
                                    {
                                        source_page = page;
                                        source_group = group;
                                        mode = opened;
                                    }
                                }
                                PanelAction::OpenStashHtml(i) => {
                                    if let Some(item) = snapshot.stash.get(i) {
                                        open_in_browser(
                                            &repo,
                                            &HtmlTarget::Stash(item.stem.clone()),
                                        );
                                    }
                                }
                                PanelAction::None => {}
                            }
                        }
                        // Detail page: a selectable action menu over one agent.
                        Mode::AgentDetail { idx, sel } => {
                            let actions = snapshot
                                .agents
                                .get(idx)
                                .map(|a| {
                                    detail_actions(
                                        a.role,
                                        reconcile_worker.presence_of(&a.label),
                                        a.session.is_some(),
                                    )
                                })
                                .unwrap_or_default();
                            // The list the operator SAW is what their
                            // cursor means; presence may have moved it
                            // since that frame.
                            let sel = match &detail_shown {
                                Some(shown) => rebind_detail_sel(shown, sel, &actions),
                                None => sel.min(actions.len().saturating_sub(1)),
                            };
                            match agent_detail_nav(sel, &actions, k) {
                                // The kill row belongs to the WAIT page; the
                                // agent page never produces it.
                                DetailNav::Quit => break 'evloop,
                                DetailNav::Back => {
                                    mode = Mode::AgentPanel {
                                        sel: row_position(
                                            &panel_row_list(&snapshot, cols as usize),
                                            crate::cli::status_tui::input::PanelRow::Agent(idx),
                                        ),
                                    }
                                }
                                DetailNav::MoveCursor(s) => {
                                    mode = Mode::AgentDetail { idx, sel: s }
                                }
                                // The confirm and what it will release are
                                // taken in this one keystroke: keys arrive
                                // in batches, and no painted frame need
                                // stand between opening a page and `u`.
                                DetailNav::Activate(DetailAction::ClearSession) => {
                                    if let Some((next, target)) = snapshot
                                        .agents
                                        .get(idx)
                                        .and_then(crate::cli::status_tui::input::open_clear_confirm)
                                    {
                                        mode = next;
                                        clear_target = Some(target);
                                    }
                                }
                                DetailNav::Activate(action) => {
                                    // Swap opens a picker, so its
                                    // candidates must be read FRESH here —
                                    // the same rule `OpenPicker` follows.
                                    // `apply_detail_action` cannot do it: it
                                    // holds neither `home` nor `picker`.
                                    if action == DetailAction::Swap {
                                        picker = crate::cli::status::available_agents(
                                            home.as_deref(),
                                            &snapshot.agents,
                                        );
                                    }
                                    let mut err: Option<Overlay> = None;
                                    let agent_row = row_position(
                                        &panel_row_list(&snapshot, cols as usize),
                                        crate::cli::status_tui::input::PanelRow::Agent(idx),
                                    );
                                    mode = apply_detail_action(
                                        action,
                                        idx,
                                        sel,
                                        &mut snapshot,
                                        &repo,
                                        &mut err,
                                        agent_row,
                                    );
                                    if err.is_some() {
                                        detail = err;
                                    }
                                    if action == DetailAction::Reopen
                                        && let Some(agent) = snapshot.agents.get(idx)
                                    {
                                        reopen_pending.push_back((agent.label.clone(), None));
                                        reconcile_worker.reopen(&agent.label);
                                    }
                                }
                                DetailNav::None => {}
                            }
                        }
                        // Picker: choose a candidate to add.
                        Mode::AddPicker { sel } => match k {
                            Key::Quit => break 'evloop,
                            // Esc/Tab close the picker back onto the +add row.
                            Key::Escape | Key::Focus | Key::Char(b'a') => {
                                picker.clear();
                                mode = Mode::AgentPanel {
                                    sel: crate::cli::status_tui::input::add_row_index(
                                        &panel_row_list(&snapshot, cols as usize),
                                    ),
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
                            // Choosing WHO is all this step does. What
                            // they review is the next question, and it
                            // is asked out loud rather than hidden on
                            // these rows' arrow keys.
                            Key::Enter => {
                                if sel < picker.len() {
                                    mode = Mode::AddReviews {
                                        candidate: sel,
                                        kind: crate::cli::teams_config::ReviewKind::Commit,
                                        sel: 0,
                                    };
                                }
                            }
                            Key::Left
                            | Key::Right
                            | Key::Space
                            | Key::PageUp
                            | Key::PageDown
                            | Key::Top
                            | Key::Bottom
                            | Key::Delete
                            | Key::Yes
                            | Key::No
                            | Key::Html
                            | Key::Plus
                            | Key::Minus
                            | Key::Char(_) => {}
                        },
                        Mode::SwapPicker { out, sel } => match k {
                            Key::Quit => break 'evloop,
                            // Backing out performs NOTHING — the roster is
                            // untouched until a candidate is chosen.
                            Key::Escape | Key::Focus => {
                                mode = Mode::AgentDetail { idx: out, sel: 0 };
                            }
                            Key::Up => {
                                mode = Mode::SwapPicker {
                                    out,
                                    sel: move_selection(sel, picker.len(), false),
                                }
                            }
                            Key::Down => {
                                mode = Mode::SwapPicker {
                                    out,
                                    sel: move_selection(sel, picker.len(), true),
                                }
                            }
                            Key::Enter => {
                                let mut err: Option<(String, String)> = None;
                                mode = apply_swap(
                                    out,
                                    sel,
                                    &mut snapshot,
                                    &picker,
                                    &repo,
                                    home.as_deref(),
                                    &mut err,
                                );
                                if let Some((title, msg)) = err {
                                    detail = Some(Overlay::error(title, msg));
                                }
                            }
                            _ => {}
                        },
                        // Confirm: one decision, resolved in one place.
                        Mode::Confirm { action } => {
                            if let ConfirmAction::ClearSession { scroll } = action
                                && let Some(target) = clear_target.as_ref()
                                && let Some(scroll) =
                                    crate::cli::status_tui::input::clear_confirm_scroll(
                                        scroll,
                                        k,
                                        crate::cli::status_tui::render::clear_confirm_max_scroll(
                                            target,
                                            reconcile_worker.presence_of(&target.label),
                                            rows as usize,
                                            cols as usize,
                                        ),
                                    )
                            {
                                mode = Mode::Confirm {
                                    action: ConfirmAction::ClearSession { scroll },
                                };
                                continue;
                            }
                            if let Some(go) = confirm_decision(action, k) {
                                if action == ConfirmAction::RestartSource {
                                    if go
                                        && let Some(request) = restart_request(
                                            source_page.as_ref(),
                                            restart_accepted.take(),
                                        )
                                    {
                                        let repo = repo.clone();
                                        let answered = tokio::task::spawn_blocking(move || {
                                            crate::cli::monitor::request(&repo, request)
                                        })
                                        .await
                                        .unwrap_or_else(|e| Err(format!("{e}")));
                                        if let Err(why) = answered {
                                            detail = Some(Overlay::error(
                                                "restart failed".to_string(),
                                                why,
                                            ));
                                        }
                                        refresh_pending = true;
                                    }
                                    mode = Mode::SourcePage { sel: 0, lift: 0 };
                                    continue;
                                }
                                if action.is_plan_page() {
                                    // Plan-page confirms execute the async
                                    // core here (the TUI confirm IS the
                                    // confirmation — the core's own prompt
                                    // is bypassed with yes:true); an Err
                                    // opens the error overlay instead of
                                    // vanishing (errors-in-the-TUI).
                                    if go
                                        && let Some(stem) =
                                            plan_page.as_ref().map(|pp| pp.stem.clone())
                                    {
                                        let outcome = run_plan_confirm(action, &repo, &stem).await;
                                        settle_plan_confirm(
                                            &repo,
                                            action,
                                            &outcome,
                                            &mut plan_page,
                                        );
                                        if let Err(e) = outcome {
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
                                // The kill is resolved and validated
                                // HERE, after the answer: the record can
                                // have been replaced and the pid reused
                                // while the confirm was on screen.
                                if action == ConfirmAction::KillAttended {
                                    if go {
                                        match wait_page
                                            .as_ref()
                                            .ok_or_else(|| "no wait is open".to_string())
                                            .and_then(|p| resolve_kill_target(p, &repo))
                                        {
                                            Ok(pid) => {
                                                // The recorded pid ALONE: its group
                                                // is unverified, and signalling one
                                                // we did not identify is the same
                                                // mistake as signalling a reused
                                                // pid.
                                                crate::signal::signal_process(pid, libc::SIGTERM);
                                                refresh_pending = true;
                                            }
                                            Err(why) => {
                                                detail = Some(Overlay::error(
                                                    "nothing was signalled".to_string(),
                                                    why,
                                                ));
                                            }
                                        }
                                    }
                                    wait_page = None;
                                    mode = Mode::AgentPanel { sel: 0 };
                                    continue;
                                }
                                if let ConfirmAction::ClearSession { .. } = action {
                                    mode = if go {
                                        refresh_pending = true;
                                        clear_target_session(
                                            &repo,
                                            &mut snapshot,
                                            clear_target.as_ref(),
                                            &mut detail,
                                        )
                                    } else {
                                        clear_target
                                            .as_ref()
                                            .and_then(|t| {
                                                relocate_detail(&t.label, &snapshot.agents)
                                            })
                                            .map_or(Mode::AgentPanel { sel: 0 }, |idx| {
                                                Mode::AgentDetail { idx, sel: 0 }
                                            })
                                    };
                                    continue;
                                }
                                let mut roster_write_succeeded = false;
                                if go {
                                    match apply_confirm(
                                        action,
                                        &repo,
                                        home.as_deref(),
                                        &snapshot,
                                        &picker,
                                    ) {
                                        Ok(()) => roster_write_succeeded = true,
                                        Err(e) => {
                                            detail = Some(Overlay::error(
                                                "roster change failed".to_string(),
                                                format!("{e:?}"),
                                            ));
                                        }
                                    }
                                }
                                let (next_mode, clear_picker) = roster_confirm_after_decision(
                                    action,
                                    roster_write_succeeded,
                                    picker.len(),
                                    crate::cli::status_tui::input::add_row_index(&panel_row_list(
                                        &snapshot,
                                        cols as usize,
                                    )),
                                    snapshot.agents.len(),
                                );
                                if roster_write_succeeded {
                                    refresh_pending = true;
                                }
                                if clear_picker {
                                    picker.clear();
                                }
                                mode = next_mode;
                            }
                        }
                        // Unreachable by construction: PlanInput parses
                        // with text_mode=true, so every key routed here
                        // is a TextKey handled above — but the match
                        // must stay exhaustive.
                        Mode::PlanInput { .. } | Mode::PauseInput | Mode::BlockPage => {}
                        // The plan-actions page (tui-plan-actions-page).
                        Mode::EventDetail { sel } => {
                            let Some(ep) = event_page.clone() else {
                                mode = Mode::LogScroll; // invariant breach — bail out
                                continue;
                            };
                            let actions = actions_for(&ep);
                            // A displayed hotkey acts DIRECTLY, leaving the
                            // selection where it was — pressing `o` must not
                            // silently re-aim what Enter would do next
                            // (tui-event-page-hotkeys).
                            let nav = match event_hotkey(k, &actions) {
                                Some(a) => EventNav::Act(a),
                                None => event_detail_nav(sel, &actions, k, page, ep.scroll),
                            };
                            match nav {
                                EventNav::Sel(x) => mode = Mode::EventDetail { sel: x },
                                EventNav::Scroll(delta) => {
                                    if let Some(live) = event_page.as_mut() {
                                        let max_off =
                                            screen_total.saturating_sub((rows as usize).max(1));
                                        live.scroll = (live.scroll as i64 + delta as i64)
                                            .clamp(0, max_off as i64)
                                            as usize;
                                    }
                                }
                                EventNav::Back => {
                                    event_page = None;
                                    mode = event_beneath.take().unwrap_or(Mode::LogScroll);
                                }
                                // The EFFECT is resolved at the
                                // tested seam (event_action_effect,
                                // codex 525cef1); the loop only
                                // performs the IO.
                                EventNav::Act(a) => match event_action_effect(&ep, a) {
                                    EventEffect::Open(url) => {
                                        let _ = crate::cli::html::launch_opener(
                                            std::path::Path::new(&url),
                                        );
                                    }
                                    EventEffect::AckFanout(keys) => {
                                        // Shared core; the refold
                                        // re-renders handled state
                                        // (best-effort: a failed
                                        // fanout leaves rows
                                        // unhandled and visible).
                                        if let Err(e) =
                                            crate::cli::events::ack_members(&repo, &keys)
                                        {
                                            detail = Some(Overlay::error(
                                                "Event acknowledgement refused".into(),
                                                format!("{e:#}"),
                                            ));
                                        }
                                        refresh_pending = true;
                                    }
                                    EventEffect::RetryContent => {
                                        // A NEW generation, so the
                                        // failed attempt cannot land
                                        // on top of this one if it
                                        // finishes late.
                                        let before = actions_for(&ep);
                                        if let Some(live) = event_page.as_mut()
                                            && let Some(slot) = live.content.as_mut()
                                        {
                                            content_generation += 1;
                                            slot.retry(content_generation);
                                            spawn_content_fetch(
                                                &ev_tx_async,
                                                live.event.source.clone(),
                                                slot.target.clone(),
                                                content_generation,
                                            );
                                        }
                                        // Retry vanishes the moment it
                                        // is pressed (Loading is not
                                        // retryable), so the cursor
                                        // must move with it.
                                        if let Some(live) = event_page.as_ref() {
                                            let after = actions_for(live);
                                            mode = Mode::EventDetail {
                                                sel: rebind_event_sel(&before, sel, &after),
                                            };
                                        }
                                    }
                                    EventEffect::Close => {
                                        event_page = None;
                                        mode = event_beneath.take().unwrap_or(Mode::LogScroll);
                                    }
                                    EventEffect::None => {}
                                },
                                EventNav::None => {}
                            }
                        }
                        Mode::CommitPage { sel } => {
                            let Some(cp) = commit_page.as_mut() else {
                                mode = Mode::LogScroll; // invariant breach — bail out
                                continue;
                            };
                            let actions = commit_actions(&cp.plan);
                            match commit_page_nav(sel, &actions, k, page, cp.scroll) {
                                CommitNav::Sel(x) => mode = Mode::CommitPage { sel: x },
                                CommitNav::Scroll(delta) => {
                                    let max_off =
                                        screen_total.saturating_sub((rows as usize).max(1));
                                    cp.scroll = (cp.scroll as i64 + delta as i64)
                                        .clamp(0, max_off as i64)
                                        as usize;
                                }
                                CommitNav::Act(CommitAction::OpenHtml) => open_in_browser(
                                    &repo,
                                    &HtmlTarget::Commit(cp.detail.sha.as_str().to_string()),
                                ),
                                CommitNav::Act(CommitAction::OpenPlan(key)) => {
                                    if let Some(pp) = open_plan_from_commit(
                                        &repo,
                                        &mut commit_page,
                                        &mut commit_beneath,
                                        &key,
                                        &snapshot,
                                    )
                                    .await
                                    {
                                        plan_page = Some(pp);
                                        mode = Mode::PlanDetail { sel: 0 };
                                    }
                                }
                                CommitNav::Act(CommitAction::Back) => {
                                    commit_page = None;
                                    mode = Mode::LogScroll;
                                }
                                CommitNav::None => {}
                            }
                        }
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
                                    mode = close_plan_page(
                                        &repo,
                                        &mut plan_page,
                                        &mut commit_beneath,
                                        &mut commit_page,
                                        &snapshot,
                                    );
                                }
                                // ONE validated mutation — the primitive
                                // `queue reprioritise` uses, never an
                                // inline rename. The page follows the
                                // write; the panel re-sorts on the refresh
                                // the rename wakes.
                                // The row opens the page that owns the
                                // value; nothing changes here.
                                PlanNav::Priority => {
                                    if let Some(live) = plan_page.as_ref()
                                        && let input::PlanPageState::Queued { priority } = live.st
                                    {
                                        mode = Mode::PriorityPage { value: priority };
                                    }
                                }
                                PlanNav::Act(PlanAction::Priority) => {
                                    if let Some(live) = plan_page.as_ref()
                                        && let input::PlanPageState::Queued { priority } = live.st
                                    {
                                        mode = Mode::PriorityPage { value: priority };
                                    }
                                }
                                PlanNav::Act(a) => match a {
                                    // A queued page opens ITS file's html —
                                    // the file stem, never the parsed name a
                                    // duplicate could share.
                                    PlanAction::OpenHtml => open_in_browser(
                                        &repo,
                                        &match (queue_html_handle(&pp), pp.st) {
                                            (Some(stem), _) => HtmlTarget::Queue(stem),
                                            (None, input::PlanPageState::Stashed) => {
                                                HtmlTarget::Stash(pp.stem.clone())
                                            }
                                            (None, _) => HtmlTarget::Plan(pp.stem.clone()),
                                        },
                                    ),
                                    // A value row activates nothing; the
                                    // nav never yields it, but the match
                                    // must say so.
                                    PlanAction::Priority => {}
                                    PlanAction::Unqueue => {
                                        mode = Mode::Confirm {
                                            action: ConfirmAction::UnqueuePlan,
                                        };
                                    }
                                    PlanAction::Promote => {
                                        mode = Mode::Confirm {
                                            action: ConfirmAction::PromotePlan,
                                        };
                                    }
                                    PlanAction::QueueDraft => {
                                        mode = Mode::Confirm {
                                            action: ConfirmAction::QueueDraft,
                                        };
                                    }
                                    PlanAction::PromoteDraft => {
                                        mode = Mode::Confirm {
                                            action: ConfirmAction::PromoteDraft,
                                        };
                                    }
                                    PlanAction::Stash => {
                                        mode = Mode::Confirm {
                                            action: ConfirmAction::StashPlan,
                                        };
                                    }
                                    PlanAction::Restore => {
                                        mode = Mode::Confirm {
                                            action: ConfirmAction::RestorePlan,
                                        };
                                    }
                                    PlanAction::DropStash => {
                                        // Counted from the RECORD, now,
                                        // not from the snapshot the page
                                        // was drawn with. The snapshot is
                                        // as old as the last refresh, and
                                        // a screen that says "3 commits"
                                        // over a stash that has since
                                        // changed is the defect
                                        // `PurgeDrop` already records.
                                        // No count, no screen.
                                        let sized = plan_page.as_ref().and_then(|pp| {
                                            crate::cli::stash::scan_stash(&repo)
                                                .into_iter()
                                                .find(|(stem, _)| *stem == pp.stem)
                                                .and_then(|(_, rec)| {
                                                    // BOTH halves captured here, with the
                                                    // count. A protective ref that will
                                                    // not resolve has no identity, so
                                                    // there is no screen — `None` falls to
                                                    // the error below rather than opening
                                                    // a confirm that cannot keep its word.
                                                    crate::cli::stash::StashIdentity::of(
                                                        &repo, &rec,
                                                    )
                                                    .map(|id| (rec.shas.len(), id))
                                                })
                                        });
                                        match sized {
                                            Some((commits, identity)) => {
                                                mode = Mode::Confirm {
                                                    action: ConfirmAction::DropStashedPlan {
                                                        commits,
                                                        identity,
                                                    },
                                                };
                                            }
                                            None => {
                                                detail = Some(Overlay::error(
                                                    "cannot size the drop".to_string(),
                                                    "no stash record for this plan — it may \
                                                     have been popped or dropped already"
                                                        .to_string(),
                                                ));
                                            }
                                        }
                                    }
                                    PlanAction::Purge => mode = Mode::PurgeChoice { sel: 0 },
                                    PlanAction::Back => {
                                        mode = close_plan_page(
                                            &repo,
                                            &mut plan_page,
                                            &mut commit_beneath,
                                            &mut commit_page,
                                            &snapshot,
                                        );
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
                                    // The same one-keystroke confirm every
                                    // other destructive action on this page
                                    // uses, defaulting to No. Typing the
                                    // stem guarded against something git
                                    // already guards against: the commits
                                    // become unreachable, not destroyed, and
                                    // the confirm names the way back.
                                    //
                                    // But only with a real number in hand.
                                    // Opening this screen without one is how
                                    // "drops 0 commit(s)" came to precede a
                                    // real drop.
                                    let sized = match plan_page.as_ref() {
                                        Some(pp) => drop_size(&repo, &pp.stem).await,
                                        None => Err(anyhow::anyhow!("no plan is open")),
                                    };
                                    match sized {
                                        Ok(commits) => {
                                            mode = Mode::Confirm {
                                                action: ConfirmAction::PurgeDrop { commits },
                                            };
                                        }
                                        Err(e) => {
                                            detail = Some(Overlay::error(
                                                "cannot size the drop".to_string(),
                                                format!(
                                                    "{e:#}\n\nNothing was changed. The drop is \
                                                     not offered without knowing what it would \
                                                     take."
                                                ),
                                            ));
                                            mode = Mode::PlanDetail { sel: 0 };
                                        }
                                    }
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
                            // Pause is REPO state: `b` is global, live
                            // even with no plans and an empty queue —
                            // an idle repo the human wants kept idle is
                            // exactly when pausing matters.
                            Key::Char(b'b') => {
                                let paused = block_page::repo_paused(&snapshot.blocks);
                                if paused {
                                    if let Err(e) = resume_from_pause(&repo, &snapshot).await {
                                        detail = Some(Overlay::error(
                                            "resume failed".to_string(),
                                            format!("{e:?}"),
                                        ));
                                    }
                                    refresh_pending = true;
                                } else {
                                    plan_input = Some(TextInput::default());
                                    mode = Mode::PauseInput;
                                }
                            }
                            Key::Focus | Key::Char(b'a') => {
                                mode = mode.toggle_focus(snapshot.agents.len())
                            }
                            Key::Char(b'r') => {
                                if let Some(n) = remote.toggle(&snapshot) {
                                    detail = Some(Overlay::notice(n));
                                }
                            }
                            Key::Up => match log_up_crossing(&log, &snapshot, cols as usize) {
                                Some(panel) => mode = panel,
                                None => log.up(total, &|i| is_rule(&snapshot, head, i)),
                            },
                            Key::Down => log.down(total, &|i| is_rule(&snapshot, head, i)),
                            Key::PageUp => {
                                log.page_up(page, total, &|i| is_rule(&snapshot, head, i))
                            }
                            Key::Space | Key::PageDown => {
                                log.page_down(page, total, &|i| is_rule(&snapshot, head, i))
                            }
                            Key::Top => log.jump_top(total, &|i| is_rule(&snapshot, head, i)),
                            Key::Bottom => log.jump_bottom(total, &|i| is_rule(&snapshot, head, i)),
                            // Enter drills into the entry under the cursor:
                            // build the same scroll sequence the cursor indexes
                            // and open the entry's page. A commit/review opens the
                            // commit page (a review scrolled to that reviewer's
                            // feedback); a plan header opens the plan page; any
                            // line of a question opens its block page.
                            Key::Enter => {
                                let seq = build_scroll(&snapshot, &ask_lines);
                                if let Some(page) = block_page::from_log(&seq, log.cursor) {
                                    block_page = Some(page);
                                    mode = Mode::BlockPage;
                                    continue;
                                }
                                if let Some(Seg::Log(
                                    row @ crate::cli::log::OnelineRow::Nudge { .. },
                                )) = seq.get(log.cursor)
                                {
                                    detail = nudge_detail(row);
                                    continue;
                                }
                                // A github row opens the EVENT PAGE
                                // (tui-github-event-page): target =
                                // the component's first member (full
                                // copy key), retained set = every
                                // member key.
                                if let Some(Seg::Log(crate::cli::log::OnelineRow::Github {
                                    event_idx,
                                    ..
                                })) = seq.get(log.cursor)
                                {
                                    if let Some(page) =
                                        snapshot.github_events.get(*event_idx).and_then(|ev| {
                                            open_event_page(
                                                &repo,
                                                ev,
                                                &mut content_generation,
                                                &ev_tx_async,
                                            )
                                        })
                                    {
                                        event_page = Some(page);
                                        event_beneath = None;
                                        mode = Mode::EventDetail { sel: 0 };
                                    }
                                    continue;
                                }
                                match entry_target(&seq, log.cursor) {
                                    Some(EntryTarget::Commit { sha, focus }) => {
                                        if let Some((cp, m)) = open_commit_page(
                                            &repo,
                                            &sha,
                                            focus.as_deref(),
                                            &snapshot,
                                            cols as usize,
                                        ) {
                                            commit_page = Some(cp);
                                            mode = m;
                                        }
                                    }
                                    // A plan header opens the ACTIONS
                                    // page (buttons over the document —
                                    // tui-plan-page-redesign).
                                    Some(EntryTarget::Plan { key }) => {
                                        if let Some(pp) =
                                            open_plan_page(&repo, &key, &snapshot).await
                                        {
                                            plan_page = Some(pp);
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
                // Entering the agents' screens asks for a fresh listing:
                // a pane moved or opened outside clank announces nothing,
                // and this is how the user gets it seen.
                if mode_shows_agents(&mode) && !was_on_agents {
                    reconcile_worker.probe();
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

        // Every turn ends here, whatever woke it. See `close_turn`.
        refresh_pending = close_turn(
            &mut waker_probe,
            std::time::Instant::now(),
            REBUILD_MIN,
            refresh_pending,
            || crate::cli::status::attendance_fingerprint(&repo),
        )
        .refresh_pending;

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
                // Capture index-addressed targets by LABEL from the OLD
                // state before rebuilding — external roster/library edits
                // can reorder rows, so kept indices could silently retarget
                // different agents/candidates. We re-locate the same labels
                // below.
                let detail_label = match mode {
                    Mode::AgentDetail { idx, .. }
                    | Mode::Confirm {
                        action: ConfirmAction::RemoveAgent { idx },
                    }
                    | Mode::SwapPicker { out: idx, .. } => {
                        snapshot.agents.get(idx).map(|a| a.label.clone())
                    }
                    _ => None,
                };
                // EVERY mode that holds a candidate by index, including
                // the step that asks what they review: a refresh there
                // without the label drops the list and sends the
                // operator back to the panel mid-choice.
                let picker_label =
                    candidate_index(mode).and_then(|i| picker.get(i).map(|c| c.label.clone()));
                let selected_draft = selected_draft(mode, &snapshot);
                let selected_member = selected_member(mode, source_group.as_ref(), &snapshot);
                let selected_timer = selected_timer(mode, &snapshot);
                let selected_source = selected_source_row(mode, source_page.as_ref(), &snapshot);
                let keep_picker = candidate_index(mode).is_some();
                // Same identity rule for a panel cursor on a QUEUE row: a
                // reprioritise renames the queue file, so THIS refresh is
                // often self-inflicted and re-sorts the list — capture the
                // selected item's NAME so the cursor follows it.
                let panel_anchor = match mode {
                    Mode::AgentPanel { sel } => {
                        Some(crate::cli::status_tui::input::capture_anchor(
                            sel,
                            &panel_row_list(&snapshot, cols as usize),
                            &snapshot.agents,
                            &snapshot
                                .stash
                                .iter()
                                .map(|i| i.stem.clone())
                                .collect::<Vec<_>>(),
                            &snapshot
                                .queue
                                .iter()
                                .map(|q| q.name.clone())
                                .collect::<Vec<_>>(),
                            &snapshot.sources,
                        ))
                    }
                    _ => None,
                };
                // A FAILED rebuild must not kill the TUI (a gix
                // status walk racing a worktree mutation is routine)
                // and must not advance the accepted signature — see
                // [`apply_refresh`].
                let held = held_entry(mode, &snapshot, cols, &log);
                let rebuilt = StatusSnapshot::build_async(
                    &repo,
                    &basename,
                    home.as_deref(),
                    policy,
                    None,
                    true,
                )
                .await
                .map_err(|e| format!("{e:#}"));
                // Restore the user's scroll depth and re-open paging in
                // case history grew; the loop top tops up the viewport.
                let adopted_at = rebuilt
                    .as_ref()
                    .ok()
                    .and_then(|fresh| fresh.log_adopted_at.as_ref())
                    .or(snapshot.log_adopted_at.as_ref());
                // Eligibility from the REBUILT state where there is
                // one, the retained snapshot where the rebuild
                // failed. Reading it from neither — which is what an
                // empty map here did — collapsed a plan on first load
                // and gave it back its header on the next refresh
                // (codex on f99d42a).
                let stands_for = rebuilt
                    .as_ref()
                    .ok()
                    .map(|fresh| fresh.log_stands_for.clone())
                    .unwrap_or_else(|| snapshot.log_stands_for.clone());
                let fresh = crate::cli::status::tui_log_with_events(
                    &repo,
                    log.window,
                    adopted_at,
                    &stands_for,
                )
                .await;
                let refresh_ok =
                    apply_refresh(&mut snapshot, fresh.rows, rebuilt, &mut refresh_failures);
                remote.observe(&snapshot);
                // Rows and events must come from the SAME read —
                // event_idx targets this list (set after apply so a
                // replaced snapshot's own build-time read never
                // misaligns them; the kept-frame arm needs them too).
                snapshot.github_events = fresh.github_events;
                snapshot.log_next = fresh.next.clone();
                log.next = fresh.next;
                refind_entry(&mut log, held, &snapshot, cols);
                if refresh_ok {
                    last_sig = sig;
                    refresh_retry_at = None;
                } else {
                    refresh_retry_at =
                        Some(std::time::Instant::now() + retry_delay(refresh_failures));
                }
                reconcile_worker.observe(&snapshot);
                log.complete = log.next.is_none();
                log.request_fill();
                if keep_picker {
                    picker =
                        crate::cli::status::available_agents(home.as_deref(), &snapshot.agents);
                } else {
                    picker.clear();
                }
                mode = match mode {
                    Mode::LogScroll => Mode::LogScroll,
                    Mode::PauseInput => Mode::PauseInput,
                    // The token has no index to rebind. A session list
                    // can shrink under the cursor, so it is clamped
                    // against the refreshed count rather than trusted.
                    Mode::TokenPage { scroll } => Mode::TokenPage { scroll },
                    Mode::PriorityPage { value } => Mode::PriorityPage { value },
                    Mode::DraftsList { sel } => Mode::DraftsList {
                        sel: input::rebind_draft_sel(
                            sel,
                            selected_draft.as_deref(),
                            &snapshot.drafts,
                        ),
                    },
                    Mode::TimersList { sel } => {
                        let timers = input::active_timers(&snapshot.sources);
                        if timers.is_empty() {
                            Mode::AgentPanel {
                                sel: input::add_row_index(&panel_row_list(
                                    &snapshot,
                                    cols as usize,
                                )),
                            }
                        } else {
                            Mode::TimersList {
                                sel: input::rebind_list_sel(sel, selected_timer.as_ref(), &timers),
                            }
                        }
                    }
                    Mode::SourceMembers { sel } => {
                        let members = source_group
                            .as_ref()
                            .map(|g| g.members(&snapshot.sources))
                            .unwrap_or_default();
                        if members.is_empty() {
                            source_group = None;
                            Mode::AgentPanel {
                                sel: input::add_row_index(&panel_row_list(
                                    &snapshot,
                                    cols as usize,
                                )),
                            }
                        } else {
                            Mode::SourceMembers {
                                sel: input::rebind_list_sel(
                                    sel,
                                    selected_member.as_ref(),
                                    &members,
                                ),
                            }
                        }
                    }
                    // The candidate is re-located by label the way the
                    // picker's is; a list that changed under the step
                    // must not add a DIFFERENT agent.
                    Mode::AddReviews {
                        candidate,
                        kind,
                        sel,
                    } => match rebind_picker_sel_by_label(
                        candidate,
                        picker_label.as_deref(),
                        &picker,
                    ) {
                        Some(candidate) => Mode::AddReviews {
                            candidate,
                            kind,
                            sel,
                        },
                        None => Mode::AgentPanel {
                            sel: crate::cli::status_tui::input::add_row_index(&panel_row_list(
                                &snapshot,
                                cols as usize,
                            )),
                        },
                    },
                    Mode::SessionsPage { sel } => Mode::SessionsPage {
                        sel: sel.min(
                            remote_page
                                .as_ref()
                                .map(|p| p.sessions.len())
                                .unwrap_or(0)
                                .saturating_sub(1),
                        ),
                    },
                    // The page holds its ask, not a place in the list,
                    // and judges it against the fresh blocks when drawn.
                    Mode::BlockPage => Mode::BlockPage,
                    // Both identities are re-LOCATED by label, never
                    // trusted as indices: a roster or library edit can
                    // reorder either list, and a stale index would swap
                    // a different pair than the one on screen. If either
                    // label is gone, the swap it described is gone too.
                    Mode::SwapPicker { sel, .. } => {
                        match (
                            detail_label
                                .as_deref()
                                .and_then(|l| relocate_detail(l, &snapshot.agents)),
                            rebind_picker_sel_by_label(sel, picker_label.as_deref(), &picker),
                        ) {
                            (Some(out), Some(sel)) => Mode::SwapPicker { out, sel },
                            _ => Mode::AgentPanel { sel: 0 },
                        }
                    }
                    // The wait page is bound to an attendance
                    // INSTANCE. Re-locate the agent by label, then
                    // demand the same task, `at`, PID and birth: a
                    // replacement record is a different wait, and
                    // silently retargeting the page at it is how a
                    // confirmed action lands on something the user
                    // never chose.
                    Mode::WaitDetail { sel, .. } => {
                        match wait_page
                            .as_ref()
                            .and_then(|p| relocate_wait(p, &snapshot.agents))
                            .map(|i| (i, sel))
                        {
                            Some((agent, sel)) => Mode::WaitDetail { agent, sel },
                            None => {
                                wait_page = None;
                                Mode::AgentPanel { sel: 0 }
                            }
                        }
                    }
                    // The plan page doesn't depend on the roster — it
                    // must survive an empty-roster refresh (its arms are
                    // below); everything agent-shaped drops to the log.
                    Mode::AgentPanel { .. } | Mode::AgentDetail { .. }
                        if snapshot.agents.is_empty() =>
                    {
                        Mode::LogScroll
                    }
                    Mode::Confirm {
                        action:
                            ConfirmAction::RemoveAgent { .. } | ConfirmAction::ClearSession { .. },
                    } if snapshot.agents.is_empty() => Mode::LogScroll,
                    Mode::AgentPanel { sel } => Mode::AgentPanel {
                        sel: panel_anchor
                            .as_ref()
                            .map(|a| {
                                crate::cli::status_tui::input::rebind_anchor(
                                    a,
                                    &panel_row_list(&snapshot, cols as usize),
                                    &snapshot.agents,
                                    &snapshot
                                        .stash
                                        .iter()
                                        .map(|i| i.stem.clone())
                                        .collect::<Vec<_>>(),
                                    &snapshot
                                        .queue
                                        .iter()
                                        .map(|q| q.name.clone())
                                        .collect::<Vec<_>>(),
                                    &snapshot.sources,
                                )
                            })
                            .unwrap_or(sel),
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
                                sel: crate::cli::status_tui::input::add_row_index(&panel_row_list(
                                    &snapshot,
                                    cols as usize,
                                )),
                            },
                        }
                    }
                    // The add picker and add confirm track ONE global-library
                    // candidate by identity: if the selected candidate is
                    // still available after a config refresh, keep the page;
                    // otherwise close to the panel.
                    Mode::AddPicker { sel } => rebind_add_picker_after_refresh(
                        sel,
                        picker_label.as_deref(),
                        &picker,
                        crate::cli::status_tui::input::add_row_index(&panel_row_list(
                            &snapshot,
                            cols as usize,
                        )),
                    ),
                    Mode::Confirm {
                        action: ConfirmAction::AddCandidate { idx, tier },
                    } => rebind_add_confirm_after_refresh(
                        idx,
                        picker_label.as_deref(),
                        &picker,
                        crate::cli::status_tui::input::add_row_index(&panel_row_list(
                            &snapshot,
                            cols as usize,
                        )),
                        tier,
                    ),
                    // Remove confirms also track the agent by identity, so a
                    // tier change/reorder cannot make "yes" remove the wrong
                    // row. If the target vanished, close to the panel.
                    Mode::Confirm {
                        action: ConfirmAction::RemoveAgent { .. },
                    } => rebind_remove_confirm_after_refresh(
                        detail_label.as_deref(),
                        &snapshot.agents,
                        crate::cli::status_tui::input::add_row_index(&panel_row_list(
                            &snapshot,
                            cols as usize,
                        )),
                    ),
                    // The clear confirm's target is the captured
                    // `clear_target`, fixed across refreshes, so a rebind
                    // under it cannot change what "yes" releases. It
                    // stays up while its agent is on the roster.
                    Mode::Confirm {
                        action: ConfirmAction::ClearSession { scroll },
                    } => match clear_target
                        .as_ref()
                        .and_then(|t| relocate_detail(&t.label, &snapshot.agents))
                    {
                        Some(_) => Mode::Confirm {
                            action: ConfirmAction::ClearSession { scroll },
                        },
                        None => Mode::AgentPanel {
                            sel: crate::cli::status_tui::input::add_row_index(&panel_row_list(
                                &snapshot,
                                cols as usize,
                            )),
                        },
                    },
                    // The plan page tracks its plan by STEM: recompute
                    // the facts against the fresh snapshot (block state /
                    // finished-ness can flip under us); a vanished plan
                    // (stashed, purged, dropped) closes the page to the
                    // log. The chooser and plan-page confirms collapse
                    // back to the page (their target may have changed).
                    // The event page retargets through its retained
                    // member-key set; a vanished component closes to
                    // the log (tui-github-event-page).
                    Mode::EventDetail { sel } => {
                        match retarget_event_page(
                            &mut event_page,
                            &snapshot.github_events,
                            &repo,
                            sel,
                            &mut content_generation,
                        ) {
                            Some(r) => {
                                if let Some((repo_slug, target, generation)) = r.fetch {
                                    spawn_content_fetch(
                                        &ev_tx_async,
                                        repo_slug,
                                        target,
                                        generation,
                                    );
                                }
                                Mode::EventDetail { sel: r.sel }
                            }
                            None => event_beneath.take().unwrap_or(Mode::LogScroll),
                        }
                    }
                    Mode::SourcePage { sel, lift } => {
                        match source_page.as_ref().and_then(|r| r.find(&snapshot.sources)) {
                            Some(source) => Mode::SourcePage {
                                sel: input::rebind_source_sel(
                                    selected_source.as_ref(),
                                    sel,
                                    &source_rows_in(source, &snapshot),
                                    &snapshot.github_events,
                                ),
                                lift,
                            },
                            None => {
                                source_page = None;
                                source_group = None;
                                Mode::AgentPanel {
                                    sel: input::add_row_index(&panel_row_list(
                                        &snapshot,
                                        cols as usize,
                                    )),
                                }
                            }
                        }
                    }
                    Mode::CommitPage { sel } => match commit_page.as_mut() {
                        Some(cp) => refresh_commit_page(&repo, cp, sel, &snapshot),
                        None => Mode::LogScroll,
                    },
                    Mode::PlanDetail { sel } => {
                        let before = plan_page.as_ref().map(|pp| plan_actions(pp.st));
                        match refetch_or_close(
                            &repo,
                            &mut plan_page,
                            &mut commit_beneath,
                            &mut commit_page,
                            &snapshot,
                        )
                        .await
                        {
                            Ok(()) => {
                                let after = plan_actions(plan_page.as_ref().expect("kept").st);
                                Mode::PlanDetail {
                                    sel: rebind_page_sel(
                                        before.as_deref().unwrap_or(&after),
                                        sel,
                                        &after,
                                    ),
                                }
                            }
                            Err(closed) => closed,
                        }
                    }
                    Mode::PurgeChoice { .. } => {
                        match refetch_or_close(
                            &repo,
                            &mut plan_page,
                            &mut commit_beneath,
                            &mut commit_page,
                            &snapshot,
                        )
                        .await
                        {
                            Ok(()) => Mode::PlanDetail { sel: 0 },
                            Err(closed) => closed,
                        }
                    }
                    // A half-typed input SURVIVES a background refresh
                    // (the watcher fires constantly in a live session —
                    // clearing it would eat the user's text); it closes
                    // only when its plan vanished.
                    Mode::PlanInput { kind } => {
                        match refetch_or_close(
                            &repo,
                            &mut plan_page,
                            &mut commit_beneath,
                            &mut commit_page,
                            &snapshot,
                        )
                        .await
                        {
                            Ok(()) => Mode::PlanInput { kind },
                            Err(closed) => {
                                plan_input = None;
                                closed
                            }
                        }
                    }
                    Mode::Confirm { action } if action.is_plan_page() => {
                        match refetch_or_close(
                            &repo,
                            &mut plan_page,
                            &mut commit_beneath,
                            &mut commit_page,
                            &snapshot,
                        )
                        .await
                        {
                            Ok(()) => Mode::PlanDetail { sel: 0 },
                            Err(closed) => closed,
                        }
                    }
                    // A refresh cancels the confirm back to the panel.
                    // `agents.len()` is not the add row once a wait is
                    // drawn — and a KILL confirm necessarily has one,
                    // so this landed on another agent or wait every
                    // time (codex on a561012).
                    Mode::Confirm { .. } => Mode::AgentPanel {
                        sel: crate::cli::status_tui::input::add_row_index(&panel_row_list(
                            &snapshot,
                            cols as usize,
                        )),
                    },
                    // The page reads the door each frame; nothing in
                    // the snapshot is its.
                    Mode::RemotePage { sel } => Mode::RemotePage { sel },
                };
                // Retitles are the worker's: derive pure glyph data
                // here, send, never touch zellij on the loop
                // (tui-reconcile-off-loop).
                reconcile_worker.update_glyphs(&snapshot);
            }
        }
    }
    remote.stop().await;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    #[test]
    fn remote_event_rows_share_safe_icon_interpretation_with_the_terminal() {
        use crate::cli::github::timeline::{Backend, DeliveryState};
        use crate::cli::log::OnelineRow;
        let fallback =
            crate::cli::log::default_badge(crate::cli::github::timeline::Backend::Command);
        for (icon, expected) in [("💬", "💬"), ("👩‍💻", "👩‍💻"), ("\n", fallback), ("", fallback)]
        {
            let row = OnelineRow::Github {
                event_idx: 0,
                delivery: DeliveryState::Woken,
                line: "event text".into(),
                unhandled: true,
                baseline: false,
                at: 100,
                backend: Backend::Command,
                glyph: Some(icon.into()),
            };
            let value = serde_json::to_value(super::ledger_row(&row)).unwrap();
            assert_eq!(value["glyph"], format!("✓ {expected}"));
            assert_eq!(value["text"], "event text");
        }
    }

    /// `y` copies the reason that was RETAINED, not the one that was
    /// painted. The painted one is wrapped to the region and clipped
    /// to its height, which is how a reason arrives useless.
    /// Replies sent to two foreign repos that share a name, and to one
    /// whose path holds a newline and an escape. Each row names its own
    /// route, whole and safe, on every surface; the stored route keeps
    /// every byte.
    #[test]
    fn foreign_routes_are_told_apart_and_drawn_safely() {
        use crate::cli::github::log::{EventLog, ReplyOperation, Transport};
        use crate::cli::github::timeline;
        use clank_core::{ids::AgentLabel, wait::MessageReplyRoute};
        let repo = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(repo.path()).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let elsewhere = dunce::canonicalize(elsewhere.path()).unwrap();
        let routes = [
            elsewhere.join("a/life"),
            elsewhere.join("b/life"),
            elsewhere.join("c/li\nfe\u{1b}[2J"),
        ];
        let mut log = EventLog::open(
            &crate::agent_store::agents_root(&root).join("worker/events"),
            "messages",
        )
        .unwrap();
        for (i, route) in routes.iter().enumerate() {
            log.append_inbox(
                Transport::Message,
                None,
                Some(&format!("original-{i}")),
                Some(10 + i as u64),
                &clank_core::wait::WaitItem::Message {
                    presentation: None,
                    handle: format!("handle-{i}"),
                    from: Some("codex".into()),
                    repo: Some(route.display().to_string()),
                    subject: format!("question {i}"),
                    body: format!("question {i}"),
                    requires_reply: false,
                    reply_route: None,
                    selector: None,
                },
            )
            .unwrap();
            let parent = log.read_rows().unwrap().unwrap().rows.remove(i);
            let op = ReplyOperation {
                id: format!("reply-{i}"),
                repo: root.clone(),
                author: AgentLabel::parse("worker").unwrap(),
                original_seq: parent.seq,
                original_ident: parent.ident,
                original_handle: format!("handle-{i}"),
                route: crate::cli::github::log::ReplyRoute::Message(MessageReplyRoute {
                    repo: route.clone(),
                    agent: AgentLabel::parse("codex").unwrap(),
                }),
                text: format!("answer {i}"),
                at: 100 + i as u64,
                acknowledge: false,
                source_name: None,
            };
            let state = log.begin_reply_operation(&op).unwrap();
            let mut destination = EventLog::open(
                &crate::agent_store::agents_root(route).join("codex/events"),
                "messages",
            )
            .unwrap();
            let receipt = destination.accept_message_delivery(&op).unwrap();
            log.accept_reply_operation(
                &state,
                &crate::cli::github::log::Acceptance::Message(receipt.clone()),
            )
            .unwrap();
            destination.release_message_receipt(&receipt).unwrap();
        }

        let snapshot = timeline::timeline_snapshot(&root);
        let sent: Vec<&timeline::MergedEvent> = snapshot
            .events
            .iter()
            .filter(|e| !e.outgoing.is_empty())
            .collect();
        assert_eq!(sent.len(), 3, "{:?}", snapshot.notices);
        let mut stored: Vec<std::path::PathBuf> = sent
            .iter()
            .map(|e| e.outgoing[0].destination.as_ref().unwrap().repo.clone())
            .collect();
        stored.sort();
        assert_eq!(stored, routes, "the route itself keeps every byte");

        let items = crate::cli::log::interleave(&[], &snapshot.events);
        let rows =
            crate::cli::log::oneline_items_rows(&items, &Default::default(), &Default::default());
        let event_rows: Vec<&crate::cli::log::OnelineRow> = rows
            .iter()
            .filter(|r| matches!(r, crate::cli::log::OnelineRow::Github { line, .. } if line.contains(" → @codex")))
            .collect();
        assert_eq!(event_rows.len(), 3);
        let lines: Vec<String> = event_rows
            .iter()
            .map(|r| match r {
                crate::cli::log::OnelineRow::Github { line, .. } => line.clone(),
                _ => unreachable!(),
            })
            .collect();
        let distinct: std::collections::BTreeSet<&String> = lines.iter().collect();
        assert_eq!(
            distinct.len(),
            3,
            "two repos named `life` read alike: {lines:?}"
        );
        for line in &lines {
            assert!(!line.chars().any(char::is_control), "{line:?}");
        }
        assert!(
            lines.iter().any(|l| l.contains("/c/li␊fe)")),
            "the hostile path is shown whole, its break made visible: {lines:?}"
        );

        let plain = crate::cli::log::oneline_plain_lines(&rows);
        let ledger = ledger_rows(&rows);
        for line in &lines {
            assert!(
                plain.iter().any(|p| p.ends_with(line.as_str())),
                "log: {line}"
            );
            assert!(ledger.iter().any(|r| &r.text == line), "remote: {line}");
        }
        for row in event_rows {
            for selected in [false, true] {
                let spans = render::log_row_spans(row, 0, &Default::default(), 300);
                let drawn = text::row_line(&spans, selected, "", 300);
                assert!(
                    !drawn.contains("\u{1b}[2J") && !drawn.contains('\n'),
                    "the drawn row carries no injected control: {drawn:?}"
                );
                let shown = crate::cli::status_tui::fixtures::visible(&drawn);
                assert!(!shown.chars().any(char::is_control), "{shown:?}");
                assert!(!shown.contains("[2J"), "{shown:?}");
            }
        }
    }

    /// One event held by two agents, one of them through two sources.
    /// Every surface — the log, the TUI row, the remote page — says the
    /// same thing about who it is for, whatever state it is in, and a
    /// narrow terminal keeps the destination and cuts the content.
    #[test]
    fn every_surface_names_each_holder_once_whatever_the_state() {
        use crate::cli::github::log::{EventLog, Transport};
        use crate::cli::github::timeline;
        let repo = tempfile::tempdir().unwrap();
        let item = clank_core::wait::WaitItem::GithubEvent {
            presentation: None,
            repo: "o/r".into(),
            event: "pr_comment".into(),
            detail: Some("review".into()),
            number: Some(7),
            title: Some("Ship it, then tell the whole team about the release".into()),
            actor: Some("alice".into()),
            url: None,
            instructions: None,
            content: None,
        };
        let mut copies = Vec::new();
        for (agent, source) in [
            ("bob", "github-o-r-aaaa"),
            ("alice", "github-o-r-aaaa"),
            ("alice", "github-o-r-bbbb"),
        ] {
            let dir = repo.path().join(".clank/agents").join(agent).join("events");
            std::fs::create_dir_all(&dir).unwrap();
            let mut log = EventLog::open(&dir, source).unwrap();
            let seq = log
                .append_inbox(Transport::Poll, Some("feed-9"), None, Some(1_000), &item)
                .unwrap();
            copies.push((log, seq));
        }
        let text = "@alice,bob  o/r#7 pr_comment/review  “Ship it, then tell the whole team about the release”";

        let mut seen = Vec::new();
        for acked in [false, true] {
            if acked {
                for (log, seq) in &mut copies {
                    log.append_ack(*seq).unwrap();
                }
            }
            let snapshot = timeline::timeline_snapshot(repo.path());
            assert_eq!(snapshot.events.len(), 1, "{:?}", snapshot.notices);
            let event = &snapshot.events[0];
            assert_eq!(event.members.len(), 3, "three copies, two holders");
            let items = crate::cli::log::interleave(&[], &snapshot.events);
            let rows = crate::cli::log::oneline_items_rows(
                &items,
                &Default::default(),
                &Default::default(),
            );
            let badge = crate::cli::log::event_badge(event);
            let state = event.delivery().glyph();

            assert_eq!(
                crate::cli::log::oneline_plain_lines(&rows),
                [format!("{state} {badge} {text}")]
            );
            let ledger = ledger_rows(&rows);
            assert_eq!(ledger[0].text, text);
            assert_eq!(
                ledger[0].glyph.as_deref(),
                Some(format!("{state} {badge}").as_str())
            );
            let spans = render::log_row_spans(&rows[0], 0, &Default::default(), 200);
            let wide =
                crate::cli::status_tui::fixtures::visible(&text::row_line(&spans, false, "", 200));
            assert!(wide.contains(text), "{wide:?}");

            let narrow_cols = 32;
            let spans = render::log_row_spans(&rows[0], 0, &Default::default(), narrow_cols);
            let narrow = crate::cli::status_tui::fixtures::visible(&text::row_line(
                &spans,
                false,
                "",
                narrow_cols,
            ));
            assert!(
                narrow.contains("@alice,bob"),
                "the destination survives a narrow row: {narrow:?}"
            );
            assert!(
                !narrow.contains("release"),
                "the content is what gets cut: {narrow:?}"
            );
            assert!(text::display_width(&narrow) <= narrow_cols);
            seen.push(state);
        }
        assert_eq!(seen, ["?", "✓✓"], "only the state mark moves");
    }

    #[test]
    fn remote_ledger_preserves_three_delivery_markers_from_actual_wal() {
        use crate::cli::github::timeline::{self, DeliveryState};
        let (repo, _, mut log, _, _) = timeline::tests::reply_fixture(false, true);
        let row = log.read_rows().unwrap().unwrap().rows.remove(0);
        let key = clank_core::wait::EventPresentation {
            source: "messages".into(),
            seq: row.seq,
            ident: row.ident.clone(),
        };
        for state in [
            DeliveryState::Unwoken,
            DeliveryState::Woken,
            DeliveryState::Acknowledged,
        ] {
            if state == DeliveryState::Woken {
                log.append_presentations(&[key.clone()], 123, || true)
                    .unwrap();
            }
            if state == DeliveryState::Acknowledged {
                log.append_checked_ack(key.seq, &key.ident).unwrap();
            }
            let snapshot = timeline::timeline_snapshot(repo.path());
            let items = crate::cli::log::interleave(&[], &snapshot.events);
            let rows = crate::cli::log::oneline_items_rows(
                &items,
                &Default::default(),
                &Default::default(),
            );
            let web = ledger_rows(&rows);
            let row = web
                .iter()
                .find(|r| r.text.contains("Original question"))
                .unwrap();
            assert_eq!(
                row.glyph.as_deref(),
                Some(format!("{} ✉", state.glyph()).as_str())
            );
            let sent = web.iter().find(|r| r.text.contains(" → @")).unwrap();
            assert_eq!(sent.glyph.as_deref(), Some("→ ✉"));
        }
    }

    #[test]
    fn copying_takes_the_whole_reason_not_the_window() {
        let long = format!(
            "not reachable through the tunnel: {}",
            (0..40)
                .map(|i| format!("cause number {i} of the chain"))
                .collect::<Vec<_>>()
                .join(": ")
        );
        let overlay = super::Overlay::error("REMOTE".to_string(), long.clone());

        let copied = super::copyable(&overlay.data).expect("a failure is copyable");
        assert_eq!(copied, long, "the retained message, whole");

        // The same reason as painted into a small region: wrapped,
        // and cut off at the bottom.
        let (painted, total) = super::render::render_error_doc("REMOTE", &long, 0, 6, 40, false);
        assert!(total > painted.len(), "the window really does clip");
        let shown: String = painted.join("");
        assert!(
            !shown.contains("cause number 39"),
            "the window drops the tail, which is why the window is not what we copy"
        );
        assert!(copied.contains("cause number 39"), "the copy keeps it");
    }

    /// One reading of the clock decides both the word on screen and
    /// the sleep that comes back to remove it. The boundary is the
    /// whole test: crossing it between two readings used to underflow
    /// `Duration` and take the TUI down over a copy, and deciding the
    /// two separately left `copied` on screen for a minute.
    #[test]
    fn the_hint_and_its_repaint_are_one_decision() {
        use std::time::{Duration, Instant};
        let at = Instant::now();

        assert_eq!(super::hint_at(None, at), super::Hint::Idle);
        assert_eq!(super::hint_at(None, at).wait(), super::OVERLAY_IDLE);

        // Fresh: says copied, and sleeps only until it stops being true.
        let fresh = super::hint_at(Some(at), at);
        assert_eq!(fresh, super::Hint::Copied(super::COPIED_FOR));
        assert!(fresh.saying_copied());
        assert_eq!(fresh.wait(), super::COPIED_FOR);

        // A hair before the boundary: still copied, and the sleep left
        // is small but never zero, so the repaint is still owed.
        let nearly = super::hint_at(Some(at), at + super::COPIED_FOR - Duration::from_nanos(1));
        assert_eq!(nearly, super::Hint::Copied(Duration::from_nanos(1)));

        // EXACTLY the boundary: idle, not a panic and not a zero sleep.
        let exact = super::hint_at(Some(at), at + super::COPIED_FOR);
        assert_eq!(exact, super::Hint::Idle);
        assert!(!exact.saying_copied());
        assert_eq!(
            exact.wait(),
            super::OVERLAY_IDLE,
            "an expired hint owes no repaint, or the overlay busy-loops"
        );

        // Past it, by a lot. The subtraction that used to panic.
        let gone = super::hint_at(Some(at), at + Duration::from_secs(600));
        assert_eq!(gone, super::Hint::Idle);

        // A clock that went backwards is not a hint from the future.
        let backwards = super::hint_at(Some(at + Duration::from_secs(5)), at);
        assert_eq!(backwards, super::Hint::Copied(super::COPIED_FOR));
    }

    /// Only a failure carries text worth taking elsewhere.
    #[test]
    fn a_link_overlay_is_not_copyable() {
        let link = super::Overlay::link("phone".to_string(), "https://x.example".to_string());
        assert_eq!(super::copyable(&link.data), None);
    }

    /// OSC 52 is never acknowledged, so the hint is the whole of the
    /// feedback: it has to say `copied`, and then stop saying it.
    #[test]
    fn the_hint_says_copied_and_then_stops() {
        let rule = |copied| {
            super::render::render_error_doc("REMOTE", "boom", 0, 6, 60, copied).0[0].clone()
        };
        assert!(rule(false).contains("y copy"), "{}", rule(false));
        assert!(!rule(false).contains("copied"), "{}", rule(false));
        assert!(rule(true).contains("copied"), "{}", rule(true));
        assert!(!rule(true).contains("y copy"), "{}", rule(true));
    }

    /// The sequence the terminal is handed: OSC 52, the `c` (clipboard)
    /// selection, the payload base64'd, terminated.
    #[test]
    fn the_sequence_is_a_well_formed_osc_52() {
        use base64::Engine as _;
        let text = "not reachable through the tunnel: dns error\nsecond line";
        let seq = crate::cli::term::osc52(text);
        let body = seq
            .strip_prefix("\x1b]52;c;")
            .and_then(|s| s.strip_suffix("\x07"))
            .unwrap_or_else(|| panic!("not an OSC 52: {seq:?}"));
        assert_eq!(
            String::from_utf8(
                base64::engine::general_purpose::STANDARD
                    .decode(body)
                    .unwrap()
            )
            .unwrap(),
            text,
            "what the terminal decodes is what the user saw"
        );
    }

    #[test]
    fn empty_log_refuses_log_focus() {
        let snapshot = crate::cli::status_tui::fixtures::two_agent_snap();
        assert!(matches!(
            super::initial_mode(&snapshot),
            super::Mode::AgentPanel { sel: 0 }
        ));

        let mut with_history = snapshot;
        with_history
            .log_rows
            .push(crate::cli::log::OnelineRow::PlainCommit {
                at: 0,
                sha: crate::lifecycle::CommitSha::parse(&format!("{:0<40}", "abc")).unwrap(),
                subject: "first".into(),
                refs: vec!["main".into()],
            });
        assert!(matches!(
            super::initial_mode(&with_history),
            super::Mode::LogScroll
        ));
    }

    fn mk_member(
        agent: &str,
        seq: u64,
        ident: &str,
        acked: bool,
    ) -> crate::cli::github::timeline::MemberRef {
        crate::cli::github::timeline::MemberRef {
            key: crate::cli::github::timeline::MemberKey {
                agent: agent.into(),
                source: "github-o-r-aaaa".into(),
                seq,
                ident: ident.into(),
            },
            woken_at: None,
            acked,
            transport: crate::cli::github::log::Transport::Poll,
            content: None,
        }
    }

    fn mk_event(
        members: Vec<crate::cli::github::timeline::MemberRef>,
    ) -> crate::cli::github::timeline::MergedEvent {
        crate::cli::github::timeline::MergedEvent {
            replies: Vec::new(),
            reply_history_pruned: false,
            outgoing: Vec::new(),
            recurrence: None,
            at: 1,
            backend: crate::cli::github::timeline::Backend::Github,
            glyph: None,
            source: "o/r".into(),
            kind: "issue_opened".into(),
            failure: false,
            detail: None,
            number: None,
            content: None,
            body: None,
            actor: None,
            url: None,
            seen_by: members.iter().map(|m| m.key.agent.clone()).collect(),
            sender: None,
            unhandled: members.iter().any(|m| !m.acked),
            baseline: false,
            members,
        }
    }

    /// A one-member page whose content slot the caller sets.
    fn mk_page() -> EventPage {
        let ev = mk_event(vec![mk_member("alpha", 1, "f:1", false)]);
        EventPage {
            target: ev.members[0].key.clone().into(),
            retained: ev.page_keys(),
            event: ev,
            prompts: Vec::new(),
            scroll: 0,
            content: None,
        }
    }

    #[test]
    fn retry_is_offered_only_where_it_can_help() {
        use clank_core::wait::ContentRef;
        use event_content::{ContentSlot, ContentState, EventBody};

        let page_with = |state: Option<ContentState>| {
            let mut ep = mk_page();
            ep.content = state.map(|st| {
                let mut slot = ContentSlot::requesting(ContentRef::Pr { number: 1 }, 1);
                slot.state = st;
                slot
            });
            ep
        };
        let offers_retry = |ep: &EventPage| actions_for(ep).contains(&EventAction::Retry);

        assert!(
            offers_retry(&page_with(Some(ContentState::Failed("500".into())))),
            "a retryable failure must offer the retry"
        );
        assert!(
            !offers_retry(&page_with(Some(ContentState::Loading))),
            "nothing to retry while the first request is in flight"
        );
        assert!(
            !offers_retry(&page_with(Some(ContentState::Unavailable("gone".into())))),
            "a deleted object must not offer a retry that can never work"
        );
        assert!(
            !offers_retry(&page_with(Some(ContentState::Ready(EventBody {
                body: "b".into(),
                author: None,
                url: None,
            })))),
            "no retry once the body is on screen"
        );
        assert!(
            !offers_retry(&page_with(None)),
            "an event that names no object has nothing to retry"
        );
    }

    #[test]
    fn the_browser_action_prefers_the_fetched_permalink() {
        use clank_core::wait::ContentRef;
        use event_content::{ContentSlot, ContentState, EventBody};

        // The record's url is the issue/PR page; the FETCHED object
        // carries the anchored comment link the record never had, and
        // that anchor is the point of opening from a comment event.
        let mut ep = mk_page();
        ep.event.url = Some("https://github.com/o/r/pull/9".into());
        assert_eq!(
            event_open_url(&ep).as_deref(),
            Some("https://github.com/o/r/pull/9"),
            "with no body yet, the record's url still opens"
        );

        let mut slot = ContentSlot::requesting(ContentRef::ReviewComment { id: 5 }, 1);
        slot.state = ContentState::Ready(EventBody {
            body: "b".into(),
            author: None,
            url: Some("https://github.com/o/r/pull/9#discussion_r5".into()),
        });
        ep.content = Some(slot);
        assert_eq!(
            event_open_url(&ep).as_deref(),
            Some("https://github.com/o/r/pull/9#discussion_r5"),
            "once fetched, the anchored permalink wins"
        );
    }

    #[test]
    fn event_action_effects_record_the_exact_url_and_fanout() {
        // codex 525cef1: the EXECUTED step is the seam — the effect
        // carries the exact URL production hands the shared opener,
        // and the exact unhandled keys the fanout receives; a no-URL
        // browser action resolves to None (belt: the menu omits it).
        let handled = crate::cli::github::timeline::MemberRef {
            acked: true,
            ..mk_member("beta", 2, "f:2", true)
        };
        let mut ev = mk_event(vec![mk_member("alpha", 1, "f:1", false), handled]);
        ev.url = Some("https://github.com/o/r/pull/12".into());
        let ep = EventPage {
            target: ev.members[0].key.clone().into(),
            retained: ev.page_keys(),
            event: ev,
            prompts: Vec::new(),
            scroll: 0,
            content: None,
        };
        assert_eq!(
            event_action_effect(&ep, EventAction::OpenBrowser),
            EventEffect::Open("https://github.com/o/r/pull/12".into()),
            "the exact event URL reaches the opener"
        );
        assert_eq!(
            event_action_effect(&ep, EventAction::Ack),
            EventEffect::AckFanout(vec![ep.event.members[0].key.clone()]),
            "only the unhandled copy fans out"
        );
        assert_eq!(
            event_action_effect(&ep, EventAction::Back),
            EventEffect::Close
        );

        let mut no_url = ep.clone();
        no_url.event.url = None;
        assert_eq!(
            event_action_effect(&no_url, EventAction::OpenBrowser),
            EventEffect::None
        );
    }

    /// AN EVENT OFFERS ITS OBJECT AND THE THREAD AROUND IT.
    ///
    /// Both, whenever both exist. One row resolving a single URL had
    /// to choose, and chose the object — so the moment a fetch
    /// succeeded, the way back to the pull request disappeared.
    ///
    /// The container is derived from repo + number + KIND, so it works
    /// for a row recorded before any of this, whose `url` is null. That
    /// is the dead-end page from the report.
    #[test]
    fn an_event_opens_its_object_and_its_container() {
        use super::input::{actions_for, event_container_url};
        let mut ev = mk_event(vec![mk_member("alpha", 1, "f:1", true)]);
        ev.kind = "pr_comment".into();
        ev.number = Some(602);
        ev.url = Some("https://github.com/o/r/pull/602#discussion_r7".into());
        let ep = EventPage {
            target: ev.members[0].key.clone().into(),
            retained: ev.page_keys(),
            event: ev,
            prompts: Vec::new(),
            scroll: 0,
            content: None,
        };

        // Two actions, two DIFFERENT addresses.
        assert_eq!(
            event_action_effect(&ep, EventAction::OpenBrowser),
            EventEffect::Open("https://github.com/o/r/pull/602#discussion_r7".into()),
        );
        assert_eq!(
            event_action_effect(&ep, EventAction::OpenContainer),
            EventEffect::Open("https://github.com/o/r/pull/602".into()),
        );
        let offered = actions_for(&ep);
        assert!(
            offered.contains(&EventAction::OpenBrowser)
                && offered.contains(&EventAction::OpenContainer),
            "a page with both must offer both: {offered:?}",
        );

        // READY, which is the state that matters: the fetch succeeded,
        // so `event_open_url` prefers the FETCHED object's link. That
        // is exactly when a single row would have dropped the
        // container, so it is where both must be offered.
        let mut ready = ep.clone();
        // An OLD row: recorded before the classifier learned to store
        // the anchored link, so its `url` is the container's. The
        // fetch supplies the anchor. If the browser action fell back
        // to the record here, both rows would open the same page and
        // the anchored link would be lost on every pre-existing event.
        ready.event.url = Some("https://github.com/o/r/pull/602".into());
        ready.content = Some(super::event_content::ContentSlot {
            target: clank_core::wait::ContentRef::ReviewComment { id: 7 },
            generation: 1,
            state: super::event_content::ContentState::Ready(super::event_content::EventBody {
                body: "looks wrong to me".into(),
                author: Some("LLFourn".into()),
                url: Some("https://github.com/o/r/pull/602#discussion_r7".into()),
            }),
        });
        let offered = actions_for(&ready);
        assert!(
            offered.contains(&EventAction::OpenBrowser)
                && offered.contains(&EventAction::OpenContainer),
            "a fetched body must not cost the page its container: {offered:?}",
        );
        let object = event_action_effect(&ready, EventAction::OpenBrowser);
        let container = event_action_effect(&ready, EventAction::OpenContainer);
        assert_eq!(
            object,
            EventEffect::Open("https://github.com/o/r/pull/602#discussion_r7".into()),
        );
        assert_eq!(
            container,
            EventEffect::Open("https://github.com/o/r/pull/602".into()),
        );
        assert_ne!(object, container, "two actions, two addresses");

        // UNAVAILABLE, on a row recorded before any of this: `url` is
        // null and the object 404s. This is the reported page.
        let mut dead = ep.clone();
        dead.event.url = None;
        dead.content = Some(super::event_content::ContentSlot {
            target: clank_core::wait::ContentRef::ReviewComment { id: 7 },
            generation: 1,
            state: super::event_content::ContentState::Unavailable(
                "no longer on GitHub (deleted, or not visible to this token)".into(),
            ),
        });
        assert_eq!(
            event_action_effect(&dead, EventAction::OpenBrowser),
            EventEffect::None,
        );
        assert_eq!(
            event_action_effect(&dead, EventAction::OpenContainer),
            EventEffect::Open("https://github.com/o/r/pull/602".into()),
            "a deleted comment still belongs to a pull request",
        );
        let offered = actions_for(&dead);
        assert!(
            offered.contains(&EventAction::OpenContainer)
                && !offered.contains(&EventAction::OpenBrowser),
            "the dead-end page offers the thread and nothing that 404s: {offered:?}",
        );

        // The KIND picks the path; a number alone cannot.
        let mut issue = ep.event.clone();
        issue.kind = "issue_comment".into();
        assert_eq!(
            event_container_url(&issue).as_deref(),
            Some("https://github.com/o/r/issues/602"),
        );

        // No number, a kind with no container, and a command source
        // each name nothing rather than a guessed address.
        let mut bare = ep.event.clone();
        bare.number = None;
        assert_eq!(event_container_url(&bare), None, "no number, no container");

        let mut push = ep.event.clone();
        push.kind = "branch_push".into();
        assert_eq!(
            event_container_url(&push),
            None,
            "a push names no container"
        );

        let mut cmd = ep.event.clone();
        cmd.backend = crate::cli::github::timeline::Backend::Command;
        assert_eq!(
            event_container_url(&cmd),
            None,
            "a command source is not a github repo",
        );
    }

    #[test]
    fn event_prompts_dedup_and_attribute() {
        // tui-github-event-page: identical member prompts collapse
        // to ONE unattributed line; differing prompts each carry
        // their agent. Joined through the same per-agent
        // presentation path as `clank events list` (sidecars here —
        // the CLI-source tier).
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        for (agent, prompt) in [("alpha", "same intent"), ("beta", "same intent")] {
            let d = crate::agent_store::agents_root(repo)
                .join(agent)
                .join("events");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("github-o-r-aaaa.meta.json"),
                format!(r#"{{"prompt":"{prompt}"}}"#),
            )
            .unwrap();
        }
        let ev = mk_event(vec![
            mk_member("alpha", 1, "f:1", false),
            mk_member("beta", 1, "f:1", false),
        ]);
        let prompts = event_prompts(repo, &ev);
        assert_eq!(prompts, vec![(None, "same intent".to_string())]);

        // Differing: attribution appears.
        let d = crate::agent_store::agents_root(repo)
            .join("beta")
            .join("events");
        std::fs::write(
            d.join("github-o-r-aaaa.meta.json"),
            r#"{"prompt":"different intent"}"#,
        )
        .unwrap();
        let prompts = event_prompts(repo, &ev);
        assert_eq!(
            prompts,
            vec![
                (Some("alpha".to_string()), "same intent".to_string()),
                (Some("beta".to_string()), "different intent".to_string()),
            ]
        );
    }

    #[test]
    fn a_retarget_that_changes_availability_keeps_the_cursor_on_its_action() {
        // codex on 4390f27: the refresh ITSELF can change
        // availability. A legacy page with [Ack, Back] and Back
        // selected gains a URL on refresh; capturing the action list
        // after the retarget would record index 1 as Ack, silently
        // moving the operator's cursor onto Ack.
        let tmp = tempfile::tempdir().unwrap();
        let mut page = Some(mk_page());
        // Starts with no URL: [Ack, Back], Back at index 1.
        if let Some(p) = page.as_mut() {
            p.event.url = None;
        }
        let before = actions_for(page.as_ref().unwrap());
        assert_eq!(before, vec![EventAction::Ack, EventAction::Back]);
        let sel = 1;

        // The refreshed component carries a URL, so OpenBrowser
        // appears ahead of everything.
        let mut refreshed = page.as_ref().unwrap().event.clone();
        refreshed.url = Some("https://github.com/o/r/pull/12".into());
        let mut generation = 0;
        let r = retarget_event_page(&mut page, &[refreshed], tmp.path(), sel, &mut generation)
            .expect("the component survives");

        let after = actions_for(page.as_ref().unwrap());
        assert_eq!(
            after,
            vec![
                EventAction::OpenBrowser,
                EventAction::Ack,
                EventAction::Back
            ]
        );
        assert_eq!(
            after[r.sel],
            EventAction::Back,
            "the cursor must follow Back, not stay on an index that now \
             means Ack"
        );
    }

    #[test]
    fn a_retarget_that_keeps_the_same_object_issues_no_request() {
        // The bounded-request property at the seam that actually
        // decides it: an unchanged target must not spawn a fetch,
        // however many refreshes arrive.
        use clank_core::wait::ContentRef;
        let tmp = tempfile::tempdir().unwrap();
        let mut page = Some(mk_page());
        if let Some(p) = page.as_mut() {
            p.content = Some(event_content::ContentSlot::requesting(
                ContentRef::Pr { number: 1 },
                1,
            ));
        }
        // The refreshed component resolves to no reference at all
        // (legacy rows), so the slot is dropped and nothing refetches
        // in a loop; then a second pass with the same state must also
        // stay quiet.
        let same = page.as_ref().unwrap().event.clone();
        let mut generation = 1;
        let first = retarget_event_page(&mut page, &[same.clone()], tmp.path(), 0, &mut generation)
            .expect("survives");
        let second = retarget_event_page(&mut page, &[same], tmp.path(), 0, &mut generation)
            .expect("survives");
        assert!(
            second.fetch.is_none(),
            "a settled target must not re-request on every refresh"
        );
        let _ = first;
    }

    #[test]
    fn an_async_availability_change_does_not_repurpose_the_cursor() {
        // codex on dcc04f9, concrete: while Loading the list is
        // [Open, Ack, Back] and the operator selects Back at index 2.
        // A Failed completion inserts Retry ABOVE Back, so without
        // rebinding, index 2 now means Retry and Enter retries instead
        // of closing the page.
        use clank_core::wait::ContentRef;
        use event_content::{ContentSlot, ContentState};

        let mut ep = mk_page();
        ep.event.url = Some("u".into());
        ep.content = Some(ContentSlot::requesting(ContentRef::Pr { number: 1 }, 1));

        let before = actions_for(&ep);
        assert_eq!(
            before,
            vec![
                EventAction::OpenBrowser,
                EventAction::Ack,
                EventAction::Back
            ]
        );
        let sel = 2;
        assert_eq!(before[sel], EventAction::Back);

        // Loading → Failed.
        if let Some(slot) = ep.content.as_mut() {
            slot.state = ContentState::Failed("500".into());
        }
        let after = actions_for(&ep);
        assert_eq!(
            after,
            vec![
                EventAction::OpenBrowser,
                EventAction::Ack,
                EventAction::Retry,
                EventAction::Back
            ]
        );
        let moved = rebind_event_sel(&before, sel, &after);
        assert_eq!(
            after[moved],
            EventAction::Back,
            "the cursor must still be on Back, not on the Retry that \
             appeared under it"
        );

        // Retry → Loading: Retry disappears from under the cursor.
        let before = after;
        let sel = 2;
        assert_eq!(before[sel], EventAction::Retry);
        if let Some(slot) = ep.content.as_mut() {
            slot.retry(2);
        }
        let after = actions_for(&ep);
        assert!(!after.contains(&EventAction::Retry));
        let moved = rebind_event_sel(&before, sel, &after);
        assert_eq!(
            after[moved],
            EventAction::Back,
            "a vanished action falls back to Back, never to something \
             that acts"
        );
    }

    #[test]
    fn a_refresh_re_requests_only_when_the_object_changed() {
        use clank_core::wait::ContentRef;
        let a = ContentRef::ReviewComment { id: 1 };
        let b = ContentRef::ReviewComment { id: 2 };

        // The property that bounds the request stream: an unchanged
        // target must issue NOTHING, however many refreshes arrive.
        assert!(
            !content_needs_refetch(Some(&a), Some(&a)),
            "an unchanged target must never re-request — the TUI \
             refreshes on every watcher event, so this is what keeps a \
             persistent failure from becoming a request storm"
        );
        // Retargeting to a different object must.
        assert!(content_needs_refetch(Some(&a), Some(&b)));
        // Gaining or losing a reference is also a change.
        assert!(content_needs_refetch(None, Some(&a)));
        assert!(content_needs_refetch(Some(&a), None));
        assert!(!content_needs_refetch(None, None));
    }

    #[test]
    fn outgoing_reply_page_survives_refresh_without_ack_or_remote_content_requests() {
        use crate::cli::github::timeline;
        let (repo, _external, _source, _target, op) = timeline::tests::reply_fixture(false, true);
        let snapshot = timeline::timeline_snapshot(repo.path());
        let event = snapshot
            .events
            .iter()
            .find(|e| !e.outgoing.is_empty())
            .unwrap();
        let mut page = Some(EventPage {
            target: event.page_keys().remove(0),
            retained: event.page_keys(),
            event: event.clone(),
            prompts: Vec::new(),
            scroll: 0,
            content: None,
        });
        assert_eq!(actions_for(page.as_ref().unwrap()), vec![EventAction::Back]);
        let mut generation = 0;
        let result =
            retarget_event_page(&mut page, &snapshot.events, repo.path(), 0, &mut generation)
                .unwrap();
        assert!(result.fetch.is_none());
        assert_eq!(generation, 0);
        assert_eq!(
            page.as_ref().unwrap().event.body.as_deref(),
            Some(op.text.as_str())
        );
        let items = vec![crate::cli::log::TimelineItem::Github(0, event)];
        let rows =
            crate::cli::log::oneline_items_rows(&items, &Default::default(), &Default::default());
        let ledger = ledger_rows(&rows);
        assert_eq!(ledger[0].glyph.as_deref(), Some("→ ✉"));
        assert!(
            ledger[0].text.starts_with('@') && ledger[0].text.contains(" → @coordinator ("),
            "{}",
            ledger[0].text
        );
        assert_eq!(
            ledger[0].page, None,
            "outbound is not a git plan/commit page"
        );
        assert!(!refetch_event_page(&mut page, &[]));
        assert!(
            page.is_none(),
            "only disappearance of retained identities closes the page"
        );
    }

    #[test]
    fn event_page_retargets_through_the_retained_set() {
        // The retarget contract (tui-github-event-page): first
        // surviving retained key wins — including across a component
        // SPLIT — the retained set refreshes from the new component,
        // and the page closes only when nothing survives.
        let a = mk_member("alpha", 1, "f:1", false);
        let b = mk_member("beta", 1, "f:1", false);
        let mut page = Some(EventPage {
            target: a.key.clone().into(),
            retained: vec![a.key.clone().into(), b.key.clone().into()],
            event: mk_event(vec![a.clone(), b.clone()]),
            prompts: Vec::new(),
            scroll: 0,
            content: None,
        });

        // Target's copy vanished; beta's survives in a fresh component.
        let fresh = vec![mk_event(vec![b.clone()])];
        assert!(refetch_event_page(&mut page, &fresh));
        let p = page.as_ref().unwrap();
        assert_eq!(
            p.target,
            b.key.clone().into(),
            "retargeted to the first surviving key"
        );
        assert_eq!(
            p.retained,
            vec![b.key.clone().into()],
            "retained set refreshed"
        );

        // SPLIT: alpha and beta now live in SEPARATE components; a
        // page retaining both follows the FIRST retained key's side.
        let mut split_page = Some(EventPage {
            target: a.key.clone().into(),
            retained: vec![a.key.clone().into(), b.key.clone().into()],
            event: mk_event(vec![a.clone(), b.clone()]),
            prompts: Vec::new(),
            scroll: 0,
            content: None,
        });
        let split = vec![mk_event(vec![b.clone()]), mk_event(vec![a.clone()])];
        assert!(refetch_event_page(&mut split_page, &split));
        let p = split_page.as_ref().unwrap();
        assert_eq!(
            p.target,
            a.key.clone().into(),
            "first-key wins picks alpha's side"
        );
        assert_eq!(p.retained, vec![a.key.clone().into()]);

        // Everything gone → closed.
        assert!(!refetch_event_page(&mut page, &[]));
        assert!(page.is_none());
    }

    #[test]
    fn retry_delay_backs_off_and_caps() {
        // The scheduling decision (codex 889c637): bounded backoff —
        // fast first retry, gentle persistent polling, hard cap.
        assert_eq!(retry_delay(1), Duration::from_secs(1));
        assert_eq!(retry_delay(2), Duration::from_secs(2));
        assert_eq!(retry_delay(3), Duration::from_secs(4));
        assert_eq!(retry_delay(6), Duration::from_secs(30));
        assert_eq!(retry_delay(100), Duration::from_secs(30));
    }

    /// The reopen answer is one notice row, replacing the last reopen
    /// notice and leaving other notices alone — the refresh notice's
    /// own rule, applied per prefix.
    #[test]
    fn a_reopen_notice_replaces_its_predecessor_and_leaves_others() {
        use crate::cli::log::OnelineRow;
        let mut rows = vec![OnelineRow::Notice("status refresh failed: x".into())];
        mount_notice(&mut rows, REOPEN_NOTICE, "reopened `r1`'s pane");
        mount_notice(
            &mut rows,
            REOPEN_NOTICE,
            "`r1` already has a pane in this tab",
        );
        let notices: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                OnelineRow::Notice(n) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            notices,
            [
                "reopen pane: `r1` already has a pane in this tab",
                "status refresh failed: x"
            ]
        );
    }

    /// Reopen stays on the page: the request goes to the worker and
    /// the answer arrives as a notice, so there is nowhere else to go.
    /// A reopen is pressed on the detail page — a dedicated full
    /// screen that never draws the log — so a failure mounted as a
    /// log notice is invisible from where it was asked for, and the
    /// refresh the reopen itself provokes clears it before the user
    /// can get back to see it.
    #[test]
    fn a_failed_reopen_is_shown_on_the_page_that_asked() {
        use zellij::ReopenOutcome as O;
        let snap = two_agent_snap(); // claude (master) + codex (commit)
        let on_codex = Mode::AgentDetail { idx: 1, sel: 0 };
        assert!(
            reopen_overlay_for("codex", &O::NotCreated, on_codex, &snap),
            "the failure reaches the page it was asked from"
        );
        for outcome in [O::NoListing, O::Unplaced, O::NotOnRoster, O::NotInSession] {
            assert!(reopen_overlay_for("codex", &outcome, on_codex, &snap));
        }
        // A success is its own answer — the pane is there.
        for outcome in [O::Reopened, O::AlreadyOpen] {
            assert!(!reopen_overlay_for("codex", &outcome, on_codex, &snap));
        }
        // Answers that arrive after the user moved on are history,
        // and belong in the log where history lives.
        assert!(
            !reopen_overlay_for("codex", &O::NotCreated, Mode::AgentPanel { sel: 0 }, &snap),
            "not while the panel is focused"
        );
        assert!(
            !reopen_overlay_for("claude", &O::NotCreated, on_codex, &snap),
            "not another agent's answer on this agent's page"
        );
        assert!(
            !reopen_overlay_for(
                "codex",
                &O::NotCreated,
                Mode::AgentDetail { idx: 9, sel: 0 },
                &snap
            ),
            "a page whose agent is gone from the roster"
        );
    }

    /// Mounting is only half: the overlay's render branch is at the
    /// TOP of the loop, so an answer mounted below it is unseen until
    /// something else wakes the pane — with nothing animating, the
    /// 60-second idle tick. The return value IS the repaint.
    #[test]
    fn a_mounted_refusal_sends_the_loop_round_again() {
        use zellij::ReopenOutcome as O;
        let mut snap = two_agent_snap();
        let mut detail: Option<Overlay> = None;
        let on_codex = Mode::AgentDetail { idx: 1, sel: 0 };

        let repaint = mount_reopen_outcomes(
            vec![("codex".to_string(), O::NotCreated)],
            on_codex,
            &mut snap,
            &mut detail,
        );
        assert!(repaint, "the loop must re-enter to draw it");
        match detail.as_ref().map(|o| &o.data) {
            Some(OverlayData::Error { title, message }) => {
                assert_eq!(title, "reopen codex");
                assert!(message.contains("codex"), "names the agent: {message}");
            }
            _ => panic!("a failure on this page is an overlay, not a log notice"),
        }
        assert!(
            !snap
                .log_rows
                .iter()
                .any(|r| matches!(r, crate::cli::log::OnelineRow::Notice(n)
                    if n.starts_with(REOPEN_NOTICE))),
            "and not also a notice nobody can see from that page"
        );

        // A success asks for nothing: no overlay, no extra paint, and
        // the log keeps the record.
        let mut detail: Option<Overlay> = None;
        let repaint = mount_reopen_outcomes(
            vec![("codex".to_string(), O::Reopened)],
            on_codex,
            &mut snap,
            &mut detail,
        );
        assert!(!repaint && detail.is_none());
        assert!(
            snap.log_rows
                .iter()
                .any(|r| matches!(r, crate::cli::log::OnelineRow::Notice(n)
                    if n.starts_with(REOPEN_NOTICE))),
            "the notice carries the success"
        );
    }

    #[test]
    fn reopen_keeps_the_detail_page_open() {
        let repo = detail_repo();
        let mut s = two_agent_snap();
        let before = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        let mode = apply_detail_action(
            DetailAction::Reopen,
            1,
            0,
            &mut s,
            repo.path(),
            &mut None,
            1,
        );
        assert_eq!(mode, Mode::AgentDetail { idx: 1, sel: 0 });
        let after = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        assert_eq!(before, after, "a reopen is not a roster write");
    }

    /// A wake behind an overlay must still reach the page beneath:
    /// the refresh flag for the flush the overlay loop skips, the fill
    /// for a resize. Keys, worker wakes and body fetches owe it
    /// nothing (an-overlay-does-not-eat-the-refresh).
    #[test]
    fn a_wake_behind_an_overlay_is_carried_to_the_page_beneath() {
        assert_eq!(
            behind_overlay(&Ev::Refresh),
            BehindOverlay {
                refresh: true,
                fill: false
            }
        );
        assert_eq!(
            behind_overlay(&Ev::Resize),
            BehindOverlay {
                refresh: false,
                fill: true
            }
        );
        for quiet in [Ev::Stdin(b"j".to_vec()), Ev::Worker] {
            assert_eq!(behind_overlay(&quiet), BehindOverlay::default());
        }
    }

    #[test]
    fn apply_refresh_keeps_frame_and_signature_on_failure() {
        // THE contract (intro 1913796): the accepted signature
        // advances ONLY on successful snapshot replacement — a
        // failed rebuild returns false (caller leaves last_sig
        // untouched, so the SAME signature stays retryable), keeps
        // the old frame, and mounts exactly one transient notice.
        use crate::cli::log::OnelineRow;
        use crate::cli::status_tui::fixtures::snap;
        let mut current = snap(None, vec![]);
        current.basename = "old-frame".into();
        let mut failures = 0u32;

        let advance = apply_refresh(
            &mut current,
            Vec::new(),
            Err("gix status item: io race".into()),
            &mut failures,
        );
        assert!(!advance, "failure must not advance the signature");
        assert_eq!(current.basename, "old-frame", "frame kept");
        assert_eq!(failures, 1);
        assert!(matches!(
            current.log_rows.first(),
            Some(OnelineRow::Notice(n)) if n.contains("status refresh failed") && n.contains("io race")
        ));

        // Repeated failure REPLACES the notice (streak text), never
        // stacks a second row.
        let fresh = current.log_rows.split_off(1);
        let advance = apply_refresh(
            &mut current,
            fresh,
            Err("gix status item: io race".into()),
            &mut failures,
        );
        assert!(!advance);
        assert_eq!(failures, 2);
        let notices = current
            .log_rows
            .iter()
            .filter(|r| matches!(r, OnelineRow::Notice(n) if n.contains("status refresh failed")))
            .count();
        assert_eq!(notices, 1, "one notice, not a stack");
        assert!(matches!(
            current.log_rows.first(),
            Some(OnelineRow::Notice(n)) if n.contains("2 in a row")
        ));

        // Success replaces the snapshot, clears the notice (fresh
        // rows), resets the streak, and advances.
        let mut next = snap(None, vec![]);
        next.basename = "new-frame".into();
        let advance = apply_refresh(&mut current, Vec::new(), Ok(next), &mut failures);
        assert!(advance, "success advances the signature");
        assert_eq!(current.basename, "new-frame");
        assert_eq!(failures, 0);
        assert!(current.log_rows.is_empty(), "notice gone with fresh rows");
    }

    use super::fixtures::*;
    use super::*;

    #[test]
    fn log_view_cursor_moves_saturate() {
        let none = |_: usize| false;
        let mut v = LogView::new();
        assert_eq!((v.cursor, v.offset), (0, 0));
        v.up(100, &none); // at top → stays
        assert_eq!(v.cursor, 0);
        v.page_up(5, 100, &none); // at top → stays
        assert_eq!(v.cursor, 0);
        v.down(100, &none);
        v.down(100, &none);
        assert_eq!(v.cursor, 2);
        v.page_down(10, 100, &none);
        assert_eq!(v.cursor, 12);
        v.jump_top(100, &none);
        assert_eq!(v.cursor, 0);
        v.jump_bottom(20, &none); // last index of 20 entries
        assert_eq!(v.cursor, 19);
        v.jump_bottom(0, &none); // empty timeline pins at 0
        assert_eq!(v.cursor, 0);
    }

    #[test]
    fn log_view_settle_shows_the_cursor_and_enter_first_resets() {
        let mut v = LogView::new();
        // Focused: cursor below the window pulls the offset down to reveal
        // it (matches scroll_to_show), and a cursor past the end clamps.
        v.cursor = 50;
        v.settle(20, true, |_| 0, |_| 10, |_| false);
        assert_eq!(v.cursor, 19, "cursor clamped into the loaded length");
        assert_eq!(v.offset, scroll_to_show(19, 0, 10, 20));
        // Unfocused: the cursor is NOT clamped; only the offset is held to
        // the last page.
        let mut u = LogView::new();
        u.cursor = 50;
        u.offset = 999;
        u.settle(20, false, |_| 0, |_| 10, |_| false);
        assert_eq!(u.cursor, 50, "unfocused leaves the cursor alone");
        assert_eq!(u.offset, 10, "offset held to the last full page");
        v.lift = 3;
        v.enter_first();
        assert_eq!((v.cursor, v.offset, v.lift), (0, 0, 0));
    }

    /// A run's end rule is a line in the log but never a place the cursor
    /// rests, after every single key: several keys handled before a paint
    /// select what the same keys one at a time do, and an Enter right
    /// after a move never finds the cursor on the rule.
    #[test]
    fn the_log_cursor_steps_over_a_run_end_rule_on_every_key() {
        let rule = |i: usize| i == 2;
        let mut batched = LogView::new();
        for _ in 0..3 {
            batched.down(7, &rule);
            assert!(!rule(batched.cursor), "no key leaves it on the rule");
        }
        let mut one_by_one = LogView::new();
        for _ in 0..3 {
            one_by_one.down(7, &rule);
            one_by_one.settle(7, true, |_| 0, |_| 10, rule);
        }
        assert_eq!(batched.cursor, one_by_one.cursor);
        assert_eq!(batched.cursor, 4);
        let mut v = LogView::new();
        v.cursor = 1;
        v.down(7, &rule);
        assert_eq!(v.cursor, 3, "the Enter that follows opens the finish");
        v.up(7, &rule);
        assert_eq!(v.cursor, 1, "up past it");
        v.cursor = 1;
        v.page_down(1, 7, &rule);
        assert_eq!(v.cursor, 3, "a page that ends on it");
        let mut top = LogView::new();
        top.jump_top(7, &|i| i == 0);
        assert_eq!(top.cursor, 1, "nothing above: the other way");
        top.jump_bottom(7, &|i| i == 6);
        assert_eq!(top.cursor, 5, "nothing below: the other way");
        let mut s = fixtures::snap(None, vec![]);
        s.log_rows = vec![
            crate::cli::log::OnelineRow::Notice("n".into()),
            crate::cli::log::OnelineRow::RunEnd,
        ];
        assert_eq!(
            (0..4).map(|i| is_rule(&s, 2, i)).collect::<Vec<_>>(),
            [false, false, false, true],
            "after two ask lines"
        );
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

    /// The page reads these facts by name, in JavaScript, where a
    /// renamed field is not an error but a blank space. Every
    /// `facts.…` path the page walks must exist in the serialized
    /// facts, and the entries the page destructures — an agent, and
    /// what it owes — are pinned by shape.
    #[test]
    fn the_page_reads_only_fields_the_facts_have() {
        use std::collections::BTreeSet;
        const PAGE: &str = clank_web::assets::PAGE;
        let mut s = fixtures::with_agents(
            fixtures::snap(
                Some(fixtures::plan_state_at(
                    "foo",
                    clank_core::plan_view::WaitingOn::MasterToContinue,
                    clank_core::wait::Handover {
                        opened: Some(1_000),
                        ..Default::default()
                    },
                )),
                vec![],
            ),
            &[("claude", crate::cli::teams_config::RosterRole::Master)],
        );
        s.agents[0].session = Some(fixtures::bound(
            clank_core::vocab::Tool::Claude,
            "bound-session",
        ));
        s.agents[0].auto_mode_explicit = Some(clank_core::vocab::AutoMode::On);
        let v = serde_json::to_value(web_facts(&s, &Default::default())).unwrap();
        assert_eq!(v["agents"][0]["session"], "bound-session");

        let keys = |x: &serde_json::Value| -> BTreeSet<String> {
            x.as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default()
        };
        let set = |ks: &[&str]| -> BTreeSet<String> { ks.iter().map(|k| k.to_string()).collect() };
        assert_eq!(
            keys(&v["agents"][0]),
            set(&[
                "label",
                "tool",
                "invocation",
                "session",
                "role",
                "auto",
                "auto_explicit",
                "owes",
                "asking",
                "last",
                "transcript"
            ]),
            "the picker and the header destructure an agent by these names"
        );
        assert_eq!(
            keys(&v["agents"][0]["owes"]),
            set(&["verb", "object", "since"]),
            "the state line destructures an obligation by these names"
        );

        // `const L = facts.ledger` is the page's one alias; a rename
        // there would hide the ledger's fields from this scan.
        let alias = "const L = facts.ledger;";
        assert!(PAGE.contains(alias), "the page's ledger alias is `{alias}`");
        // The page's own block; its open tag carries the nonce the
        // policy admits it by (the-credential-is-not-a-cdns-to-take).
        let script = &PAGE[PAGE
            .find("<script nonce")
            .expect("the page has a script of its own")..];
        let mut walked = 0;
        for (root, base) in [("facts.", &v), ("L.", &v["ledger"])] {
            for (i, _) in script.match_indices(root) {
                // `HTML.` and the like are not the alias.
                if i > 0
                    && script[..i]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_alphanumeric() || c == '_')
                {
                    continue;
                }
                let mut rest = &script[i + root.len()..];
                let mut at = base;
                let mut path = Vec::new();
                loop {
                    let seg: String = rest
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if seg.is_empty() {
                        break;
                    }
                    rest = &rest[seg.len()..];
                    let Some(map) = at.as_object() else { break };
                    assert!(
                        map.contains_key(&seg),
                        "the page reads {root}{}{seg}, which the facts do not have",
                        path.iter().map(|s| format!("{s}.")).collect::<String>()
                    );
                    at = &map[&seg];
                    path.push(seg);
                    walked += 1;
                    if let Some(tail) = rest.strip_prefix('.') {
                        rest = tail;
                    } else {
                        break;
                    }
                }
            }
        }
        assert!(walked > 8, "the scan found almost nothing to check");
    }

    /// The agents and the ledger are the TUI's own derivations as
    /// The queue reaches the page as ENTRIES, in the order it will
    /// run them, each named by its file — a count cannot be acted on,
    /// and a parsed name is not an address
    /// (the-queue-is-workable-from-the-phone).
    #[test]
    fn the_facts_carry_the_queue_in_the_order_it_runs() {
        use crate::cli::status_tui::fixtures::snap;
        let mut s = snap(None, vec!["later", "sooner"]);
        s.queue[0].priority = 900;
        s.queue[0].path = std::path::PathBuf::from("/r/.clank/queue/900-later.md");
        s.queue[1].priority = 100;
        s.queue[1].path = std::path::PathBuf::from("/r/.clank/queue/100-sooner.md");
        s.queue.sort_by_key(|q| q.priority);
        s.stash.push(crate::cli::status::StashItemView {
            stem: "later".to_string(),
            waiting_for: None,
            ready: true,
            commits: 2,
        });

        let facts = web_facts(&s, &Default::default());
        let queued: Vec<(&str, u16, &str, &str)> = facts
            .ledger
            .queue
            .iter()
            .map(|q| {
                (
                    q.name.as_str(),
                    q.priority,
                    q.handle.as_str(),
                    q.page.as_str(),
                )
            })
            .collect();
        assert_eq!(
            queued,
            [
                ("sooner", 100, "100-sooner", "queue/100-sooner.html"),
                ("later", 900, "900-later", "queue/900-later.html"),
            ],
            "the order the queue runs them, each with its exact page"
        );
        assert_eq!(facts.ledger.stash_entries[0].page, "stash/later.html");

        // The count the page shows is these entries; there is no
        // second number to keep true.
        let v = serde_json::to_value(&facts).unwrap();
        assert_eq!(v["ledger"]["queue"].as_array().unwrap().len(), 2);
    }

    /// data — asked of the same functions, so the two surfaces cannot
    /// drift apart. One list, master-first: two reviewers missing on
    /// one commit each carry what they owe, on the sha they owe it
    /// on, with their own clock; an agent the gate is not waiting on
    /// owes nothing.
    #[test]
    fn the_web_facts_are_the_tuis_own_derivation() {
        use crate::cli::status_tui::fixtures::{plan_state_at, with_agents};
        use crate::cli::teams_config::RosterRole;
        use clank_core::ids::AgentLabel;
        let missing = clank_core::repo_state::NonEmptyVec::new(vec![
            AgentLabel::parse("codex").unwrap(),
            AgentLabel::parse("ruthless").unwrap(),
        ])
        .unwrap();
        let mut s = with_agents(
            crate::cli::status_tui::fixtures::snap(
                Some(plan_state_at(
                    "foo",
                    clank_core::plan_view::WaitingOn::ReviewerApprovalsMissing { missing },
                    clank_core::wait::Handover {
                        opened: Some(1_000),
                        ..Default::default()
                    },
                )),
                vec![],
            ),
            &[
                ("codex", RosterRole::Commit),
                ("claude", RosterRole::Master),
                ("ruthless", RosterRole::Commit),
            ],
        );
        s.log_rows = vec![crate::cli::log::OnelineRow::Review {
            at: Some(crate::age::now() - 120),
            verdict: clank_core::vocab::Verdict::Continue,
            author: "ruthless".into(),
            summary: "fine".into(),
        }];
        let with = ["codex".to_string()].into_iter().collect();
        let facts = web_facts(&s, &with);
        let (lamp, plan) = render::bar_text(&s);
        assert_eq!((facts.lamp.clone(), facts.plan.clone()), (lamp, plan));
        assert_eq!(facts.hue, derive::state_color(&s).hex());
        assert_eq!(
            facts.project, s.basename,
            "the page says which repo it is about, in the repo's own name"
        );

        let agents: Vec<(&str, &str, Option<(&str, &str, Option<i64>)>, bool)> = facts
            .agents
            .iter()
            .map(|p| {
                (
                    p.label.as_str(),
                    p.role.as_str(),
                    p.owes
                        .as_ref()
                        .map(|o| (o.verb.as_str(), o.object.as_str(), o.since)),
                    p.transcript,
                )
            })
            .collect();
        assert_eq!(
            agents,
            vec![
                ("claude", "master", None, false),
                (
                    "codex",
                    "commit",
                    Some(("reviewing", "abc1230", Some(1_000))),
                    true
                ),
                (
                    "ruthless",
                    "commit",
                    Some(("reviewing", "abc1230", Some(1_000))),
                    false
                ),
            ],
            "master first, then roster order; what each owes is on its own entry, \
             with the clock `auto` picks the oldest by; transcript per agent"
        );
        assert_eq!(facts.correction, None);
        assert_eq!(
            facts.agents[2].last.as_deref(),
            Some("✓ continued, 2m ago"),
            "a reviewer's last act is its newest verdict, aged"
        );
        assert_eq!(facts.ledger.plan.as_deref(), Some("foo"));
        assert_eq!(facts.ledger.gate.as_deref(), Some("unreviewed"));
        let row = &facts.ledger.rows[0];
        assert_eq!(
            (
                row.kind.as_str(),
                row.author.as_deref(),
                row.verdict.as_deref(),
                row.age.as_deref()
            ),
            ("review", Some("ruthless"), Some("continued"), Some("2m")),
            "the ledger is the TUI's rows with the TUI's ages"
        );
        // And the class the page colours it by. Separate from the
        // word: a row that carried the word alone would leave the
        // page with nothing to colour by, and every assertion about
        // the stylesheet would be about a class nothing wears.
        assert_eq!(
            row.verdict_class.as_deref(),
            Some("v-continue"),
            "a review row carries the class its verdict colours by",
        );

        // A broken HEAD tag belongs to the session, not to an agent,
        // and reaches the page as the sentence `clank status` prints:
        // the commit to amend and what the tag got wrong.
        let mut broken = s;
        broken.head_correction = Some(clank_core::wait::HeadCorrection {
            sha: clank_core::ids::CommitSha::parse(&format!("{:0<40}", "dec0de")).unwrap(),
            violation: clank_core::wait::HeadTagViolation {
                unknown: None,
                untagged_touched: vec![clank_core::ids::PlanKey::parse("foo").unwrap()],
                extra_named: None,
            },
        });
        let c = web_facts(&broken, &with)
            .correction
            .expect("a broken tag reaches the page");
        assert!(
            c.contains("dec0de") && c.contains("foo"),
            "the correction names the commit to amend and the plan the tag missed: {c}"
        );
    }

    /// The remote page is told how many drafts there are, and nothing
    /// that would let it act on them.
    #[test]
    fn web_facts_count_the_drafts() {
        let mut s = fixtures::snap(None, vec![]);
        assert_eq!(web_facts(&s, &Default::default()).ledger.drafts, 0);
        s.drafts = vec![crate::cli::queue::Draft {
            name: "idea".into(),
            path: "/r/.clank/drafts/idea.md".into(),
            title: Some("An idea".into()),
            modified: None,
        }];
        let ledger = serde_json::to_value(web_facts(&s, &Default::default()).ledger).unwrap();
        assert_eq!(ledger["drafts"], 1);
        assert!(!ledger.to_string().contains("An idea"), "{ledger}");
    }

    #[test]
    fn web_facts_carry_only_pending_blocks_with_their_identity() {
        let mut snap = crate::cli::status_tui::fixtures::snap(None, vec![]);
        snap.blocks = vec![
            crate::cli::block::BlockEntry {
                agent: "claude".into(),
                name: "which".into(),
                question: "Which option?".into(),
                answer: None,
                occurrence: None,
            },
            crate::cli::block::BlockEntry {
                agent: "codex".into(),
                name: "done".into(),
                question: "Why?".into(),
                answer: Some("Because".into()),
                occurrence: None,
            },
        ];
        let facts = web_facts(&snap, &Default::default());
        assert_eq!(
            facts.ledger.blocks,
            vec![PendingBlock {
                agent: "claude".into(),
                name: "which".into(),
                occurrence: None,
                question: "Which option?".into(),
            }]
        );
    }

    /// What the drop screen says it will take, asked of real repo
    /// states — active, finished-as-one-commit, and finished
    /// unsquashed. The count used to come from a snapshot map built
    /// only from ACTIVE plans, so every finished plan's screen said
    /// `0 commit(s)` and then dropped its real commits (codex on
    /// 3ed8a39); a test that injected the number could not see that.
    #[tokio::test]
    async fn the_drop_screen_counts_active_and_finished_plans_alike() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        let write = |rel: &str, body: &str| {
            let p = repo.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        write("README", "base\n");
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);

        // An ACTIVE plan of two commits.
        write(".clank/plans/live.md", "# live\n");
        git(&["add", "-A"]);
        git(&["commit", "-qm", "[live] intro"]);
        write("src/live.rs", "// live\n");
        git(&["add", "-A"]);
        git(&["commit", "-qm", "[live] implement"]);
        assert_eq!(
            drop_size(repo, "live").await.unwrap(),
            2,
            "an active plan counts its own commits"
        );

        // End this run before introducing the next historical example.
        std::fs::create_dir_all(repo.join(".clank/finished")).unwrap();
        std::fs::rename(
            repo.join(".clank/plans/live.md"),
            repo.join(".clank/finished/live.md"),
        )
        .unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "[live] finish"]);

        // A FINISHED plan collapsed to ONE commit: the same commit
        // introduces the plan and finishes it.
        write(".clank/finished/solo.md", "# solo\n");
        git(&["add", "-A"]);
        git(&["commit", "-qm", "[solo] all of it"]);
        assert_eq!(
            drop_size(repo, "solo").await.unwrap(),
            1,
            "a squashed finished plan is one commit, not zero"
        );

        // A FINISHED plan that was NOT squashed: intro, work, finish.
        write(".clank/plans/pair.md", "# pair\n");
        git(&["add", "-A"]);
        git(&["commit", "-qm", "[pair] intro"]);
        write("src/pair.rs", "// pair\n");
        git(&["add", "-A"]);
        git(&["commit", "-qm", "[pair] implement"]);
        std::fs::rename(
            repo.join(".clank/plans/pair.md"),
            repo.join(".clank/finished/pair.md"),
        )
        .unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "[pair] finish"]);
        assert_eq!(
            drop_size(repo, "pair").await.unwrap(),
            3,
            "an unsquashed finished plan counts all of them"
        );
    }

    /// No number, no screen. A rebuild or preview that fails while the
    /// drop screen opens must not become `commits: 0` — `run_plan_confirm`
    /// reruns both after `y`, so a transient failure would let a real
    /// drop follow a screen that advertised none (codex on 230585f).
    #[tokio::test]
    async fn a_drop_with_no_preview_is_refused_not_zeroed() {
        let dir = tempfile::tempdir().unwrap();
        // Not a git repo at all: the rebuild cannot answer.
        assert!(
            drop_size(dir.path(), "whatever").await.is_err(),
            "an unreadable repo is an error, never a count"
        );

        // A real repo, but no such plan — the preview has nothing to
        // size, and that is still not zero.
        let repo = dir.path();
        let git = |args: &[&str]| {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap();
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("README"), "base\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);
        assert!(
            drop_size(repo, "no-such-plan").await.is_err(),
            "a plan the preview cannot find is an error, never a count"
        );

        // And a name that is not a plan name never reaches the repo.
        assert!(drop_size(repo, "not/a/stem").await.is_err());
    }

    /// A collapsed row is still a COMMIT to everything that navigates
    /// by commits: a review above it opens its own sha, and the row
    /// keeps its own commit page. Only the words change.
    #[test]
    fn a_collapsed_row_still_owns_its_sha_and_its_reviews() {
        use crate::cli::log::{OnelineRow, RowMarker};
        let sha = clank_core::ids::CommitSha::parse(&format!("{:0<40}", "abc")).unwrap();
        let rows = vec![
            OnelineRow::Review {
                verdict: clank_core::vocab::Verdict::Finished,
                author: "codex".into(),
                summary: "ship it".into(),
                at: Some(crate::age::now() - 60),
            },
            OnelineRow::Commit {
                sha: sha.clone(),
                at: crate::age::now() - 120,
                subject: "[solo] did the thing".into(),
                marker: RowMarker::Finish,
                stands_for: Some("solo".into()),
            },
        ];
        let ledger = ledger_rows(&rows);
        let commit = ledger
            .iter()
            .find(|r| r.kind == "commit")
            .expect("the collapsed row is still a commit");
        assert_eq!(
            commit.text, "solo",
            "it says the plan's name, not the finish message"
        );
        assert_eq!(
            commit.stands_for.as_deref(),
            Some("solo"),
            "and says WHY it says that, so the page can give the name the \
             emphasis the header it replaced had"
        );
        assert_eq!(commit.sha.as_deref(), Some(&sha.as_str()[..7]));
        assert_eq!(
            commit.page.as_deref(),
            Some(format!("commit/{}.html", sha.as_str()).as_str()),
            "and keeps its own commit page, not the plan's"
        );
        let review = ledger
            .iter()
            .find(|r| r.kind == "review")
            .expect("the review survived");
        assert_eq!(
            review.page, commit.page,
            "a review above it still attributes to that commit"
        );
    }

    /// Every row that has a page names it: a plan umbrella its plan
    /// page, a commit its commit page (pre-adoption ones too — the
    /// server renders those on request), a review — which sits above
    /// its commit in the newest-first log — the page of the commit
    /// BELOW it and not the one above, never one from past an
    /// umbrella; the ad-hoc umbrella and github rows none.
    /// Attribution from PRODUCER rows: the fixture above places the
    /// name on top, so it cannot tell whether the ledger still
    /// attributes correctly with the name under its run (codex on
    /// 51fb3c8).
    ///
    /// Two plans in sequence. Each review must carry its own commit's
    /// page, and each name its own plan's.
    #[test]
    fn ledger_attribution_survives_the_name_moving_under_its_run() {
        use crate::cli::log::{OnelineRow, oneline_rows};
        use crate::lifecycle::{CommitSha, PlanKey};
        use clank_core::repo_state::LogEvent;
        let sha = |s: &str| CommitSha::parse(&format!("{:0<40}", s)).unwrap();
        let commit = |plan: &str, s: &str, ts: i64| LogEvent::PlanCommit {
            plan: PlanKey::parse(plan).unwrap(),
            sha: sha(s),
            ts,
            touched_plan: false,
            touched_code: true,
            subject: format!("[{plan}] work"),
        };
        let events = [
            commit("bee", "b2", 4),
            commit("bee", "b1", 3),
            commit("ant", "a2", 2),
            commit("ant", "a1", 1),
        ];
        let refs: Vec<&LogEvent> = events.iter().collect();
        let mut reviews = std::collections::BTreeMap::new();
        for s in ["b2", "a2"] {
            reviews.insert(
                sha(s).as_str().to_string(),
                vec![crate::cli::log::Review {
                    at: None,
                    author: "codex".into(),
                    verdict: clank_core::vocab::Verdict::Continue,
                    summary: "ok".into(),
                    body: String::new(),
                }],
            );
        }
        let rows = oneline_rows(&refs, &reviews, &Default::default());
        assert!(
            matches!(rows.last(), Some(OnelineRow::Header { plan: Some(p), .. }) if p == "ant"),
            "the producer puts the name last: {rows:?}"
        );
        let page = |s: &str| Some(format!("commit/{}.html", sha(s).as_str()));
        let pages: Vec<Option<String>> = ledger_rows(&rows).into_iter().map(|r| r.page).collect();
        assert_eq!(
            pages,
            vec![
                page("b2"), // the review above b2
                page("b2"),
                page("b1"),
                Some("plan/bee.html".into()),
                page("a2"), // and this review belongs to the run BELOW the name
                page("a2"),
                page("a1"),
                Some("plan/ant.html".into()),
            ],
            "{rows:?}"
        );
    }

    #[test]
    fn ledger_rows_name_their_pages() {
        use crate::cli::log::{OnelineRow, RowMarker};
        let sha = |c: char| crate::lifecycle::CommitSha::parse(&c.to_string().repeat(40)).unwrap();
        let review = || OnelineRow::Review {
            at: None,
            verdict: clank_core::vocab::Verdict::Continue,
            author: "codex".into(),
            summary: "ok".into(),
        };
        let rows = vec![
            OnelineRow::Header {
                duration_secs: None,
                starts_here: true,
                plan: Some("foo".into()),
            },
            review(),
            OnelineRow::Commit {
                sha: sha('a'),
                at: 0,
                subject: "one".into(),
                marker: RowMarker::Plain,

                stands_for: None,
            },
            review(),
            OnelineRow::Header {
                duration_secs: None,
                starts_here: true,
                plan: None,
            },
            review(),
            OnelineRow::PlainCommit {
                sha: sha('b'),
                at: 0,
                subject: "two".into(),
                refs: vec![],
            },
        ];
        let pages: Vec<Option<String>> = ledger_rows(&rows).into_iter().map(|r| r.page).collect();
        let a = Some(format!("commit/{}.html", "a".repeat(40)));
        let b = Some(format!("commit/{}.html", "b".repeat(40)));
        assert_eq!(
            pages,
            vec![
                Some("plan/foo.html".into()),
                a.clone(),
                a,
                None,
                None,
                b.clone(),
                b,
            ],
            "the review above `a` is a's; the one below it is an older commit's, not shown"
        );
    }

    /// An elapsed on screen costs the pane nothing: it is recomputed
    /// by a paint that was already going to happen, so the cadence is
    /// still decided by the spinner alone. And the idle cadence is
    /// what keeps a minute-resolution age true — a repaint no rarer
    /// than the smallest unit shown.
    #[test]
    fn a_clock_on_screen_arms_nothing_of_its_own() {
        assert_eq!(paint_interval(true), SPINNER_TICK);
        assert_eq!(
            paint_interval(false),
            IDLE_TICK,
            "ages do not shorten the idle sleep"
        );
        assert!(
            IDLE_TICK.as_secs() as i64 <= crate::age::COARSE_RESOLUTION_SECS,
            "a {}s repaint cannot keep a {}s unit honest",
            IDLE_TICK.as_secs(),
            crate::age::COARSE_RESOLUTION_SECS
        );
    }

    /// One significant token of Rust source.
    #[derive(Debug, PartialEq, Eq)]
    struct Tok<'a> {
        text: &'a str,
        at: usize,
    }

    /// Production source as the TOKENS it is made of.
    ///
    /// The point is that every part of the wiring witness below reads
    /// the same representation. The previous version counted braces
    /// through a literal-aware scanner but LOCATED its landmarks with
    /// `find` on raw text, and those two disagree: wrapping the whole
    /// production assignment in `/* … */` stopped it executing while
    /// the gate still found it, parsed its parentheses and read its
    /// fields — restoring exactly the missing-call defect this is for.
    /// The same split rejected valid code, because a comment inserted
    /// between `status::` and `attendance_fingerprint` is the same
    /// dependency and failed a `contains` (codex on 8045e76).
    ///
    /// So comments are DROPPED, not skipped-while-counting, and each
    /// string or char literal collapses to one opaque token: a literal
    /// containing an example call must not witness a call either.
    /// Whitespace between tokens stops mattering by construction,
    /// which is what makes a reflow harmless.
    fn tokens(src: &str) -> Vec<Tok<'_>> {
        let b = src.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if c.is_ascii_whitespace() {
                i += 1;
            } else if c == b'/' && b.get(i + 1) == Some(&b'/') {
                i += b[i..]
                    .iter()
                    .position(|c| *c == b'\n')
                    .unwrap_or(b.len() - i);
            } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
                // Rust block comments nest.
                let mut depth = 1;
                i += 2;
                while i < b.len() && depth > 0 {
                    if b[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if b[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            } else if let Some(next) = literal_end(b, i) {
                out.push(Tok {
                    text: "<lit>",
                    at: i,
                });
                i = next;
            } else if c.is_ascii_alphanumeric() || c == b'_' {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                out.push(Tok {
                    text: &src[start..i],
                    at: start,
                });
            } else {
                out.push(Tok {
                    text: &src[i..i + 1],
                    at: i,
                });
                i += 1;
            }
        }
        out
    }

    /// Just past a string, byte-string, raw-string or char literal
    /// starting at `i`, if one does.
    ///
    /// A lifetime is not a char literal: `'a` must stay two tokens, or
    /// everything after it on the line is swallowed.
    fn literal_end(b: &[u8], i: usize) -> Option<usize> {
        let mut j = i;
        if b[j] == b'b' {
            j += 1;
        }
        if b.get(j) == Some(&b'r') {
            let mut k = j + 1;
            let mut hashes = 0;
            while b.get(k) == Some(&b'#') {
                hashes += 1;
                k += 1;
            }
            if b.get(k) == Some(&b'"') {
                let close: Vec<u8> = std::iter::once(b'"')
                    .chain(std::iter::repeat_n(b'#', hashes))
                    .collect();
                let mut m = k + 1;
                while m < b.len() && !b[m..].starts_with(&close) {
                    m += 1;
                }
                return Some((m + close.len()).min(b.len()));
            }
            return None;
        }
        match b.get(j) {
            Some(b'"') => {
                let mut m = j + 1;
                while m < b.len() && b[m] != b'"' {
                    m += if b[m] == b'\\' { 2 } else { 1 };
                }
                Some((m + 1).min(b.len()))
            }
            Some(b'\'') => char_end(b, j),
            _ => None,
        }
    }

    /// Just past the char literal whose opening `'` is at `j`, or
    /// `None` when that quote starts a LIFETIME instead.
    ///
    /// Scanned to the closing quote rather than assumed to be one
    /// escape wide. `'\\n'` is four bytes but `'\\x7d'` is six and
    /// `'\\u{7d}'` is eight — and both of those encode a BRACE, so
    /// mis-lexing one emits a stray `}` that moves `pair_end`'s block
    /// extent and with it the receive's end (ruthless on d1b9375).
    ///
    /// The lifetime case is why this cannot simply scan for the next
    /// quote: `&'a str, '}'` would otherwise read as one literal
    /// spanning both.
    fn char_end(b: &[u8], j: usize) -> Option<usize> {
        match b.get(j + 1)? {
            // An escape. Bounded: the longest is `'\\u{10FFFF}'`.
            b'\\' => {
                // From j+3, not j+2: the character right after the
                // backslash belongs to the escape, so in `'\\''` it is
                // the quote being escaped and not the terminator.
                let mut m = j + 3;
                while m < b.len() && m <= j + 13 {
                    if b[m] == b'\'' {
                        return Some(m + 1);
                    }
                    m += 1;
                }
                None
            }
            // One ASCII character.
            _ if b.get(j + 2) == Some(&b'\'') => Some(j + 3),
            // One multi-byte character, whose width its lead byte gives.
            lead if *lead >= 0x80 => {
                let wide = match lead {
                    0xF0..=0xF7 => 4,
                    0xE0..=0xEF => 3,
                    _ => 2,
                };
                (b.get(j + 1 + wide) == Some(&b'\'')).then_some(j + 2 + wide)
            }
            // `'a` and friends: a lifetime or a loop label.
            _ => None,
        }
    }

    /// Where `pattern` occurs as a run of tokens, if it does.
    fn find_seq(toks: &[Tok<'_>], pattern: &[&str]) -> Option<usize> {
        toks.windows(pattern.len())
            .position(|w| w.iter().zip(pattern).all(|(t, p)| t.text == *p))
    }

    /// The token index just past the `lo`…`hi` pair opened at `open`.
    fn pair_end(toks: &[Tok<'_>], open: usize, lo: &str, hi: &str) -> Option<usize> {
        let mut depth = 0usize;
        for (k, t) in toks.iter().enumerate().skip(open) {
            if t.text == lo {
                depth += 1;
            } else if t.text == hi {
                depth -= 1;
                if depth == 0 {
                    return Some(k + 1);
                }
            }
        }
        None
    }

    /// Net brace depth across the tokens in `range`.
    fn depth_of(toks: &[Tok<'_>]) -> isize {
        toks.iter().fold(0isize, |d, t| match t.text {
            "{" => d + 1,
            "}" => d - 1,
            _ => d,
        })
    }

    /// The tokeniser the wiring witness rests on, checked on the
    /// decoys that would otherwise witness a call that cannot run.
    #[test]
    fn the_tokeniser_sees_only_executable_code() {
        let texts = |s: &str| {
            tokens(s)
                .into_iter()
                .map(|t| t.text.to_string())
                .collect::<Vec<_>>()
        };

        assert_eq!(texts("a = b(c);"), ["a", "=", "b", "(", "c", ")", ";"]);
        // Whitespace and comments BETWEEN tokens are insignificant, so
        // a reflow or an inserted note cannot change the match.
        assert_eq!(
            texts("crate :: cli /* note */ :: status"),
            ["crate", ":", ":", "cli", ":", ":", "status"]
        );
        assert_eq!(texts("a\n  .b()"), ["a", ".", "b", "(", ")"]);

        // A commented-out call contributes NOTHING. This is the false
        // green: the call no longer executes, and must no longer be
        // findable.
        assert!(texts("/* x = close_turn(y); */").is_empty());
        assert!(texts("// x = close_turn(y);").is_empty());
        assert_eq!(texts("/* a /* b */ c */ d"), ["d"], "block comments nest");

        // A literal containing an example call is one opaque token, so
        // it cannot witness one either.
        assert_eq!(
            texts(r#"let s = "close_turn(x)";"#),
            ["let", "s", "=", "<lit>", ";"]
        );
        assert_eq!(texts("let c = '}';"), ["let", "c", "=", "<lit>", ";"]);
        assert_eq!(
            texts(r##"let s = r#"close_turn(x)"#;"##),
            ["let", "s", "=", "<lit>", ";"],
            "a RAW literal collapses too, whatever its hash count"
        );

        // Escaped char literals of every width. `'\\x7d'` and
        // `'\\u{7d}'` are a closing BRACE, so mis-lexing one emits a
        // stray `}` and moves every block measurement after it.
        for lit in [
            "'\\n'",
            "'\\''",
            "'\\x7d'",
            "'\\u{7d}'",
            "'é'",
            "'\u{1f600}'",
        ] {
            assert_eq!(
                texts(&format!("let c = {lit};")),
                ["let", "c", "=", "<lit>", ";"],
                "{lit} must be ONE token"
            );
        }
        assert_eq!(
            depth_of(&tokens("if x { let c = '\\u{7d}'; }")),
            0,
            "a braces-encoding escape must not look like a brace"
        );

        // A lifetime is not a literal, or the rest of the line is lost.
        assert_eq!(
            texts("fn f<'a>(x: &'a str) {}"),
            [
                "fn", "f", "<", "'", "a", ">", "(", "x", ":", "&", "'", "a", "str", ")", "{", "}"
            ]
        );

        // Depth and block extent are over the same tokens.
        let t = tokens("if x { a; } b");
        assert_eq!(depth_of(&t), 0);
        let open = t.iter().position(|k| k.text == "{").unwrap();
        assert_eq!(t[pair_end(&t, open, "{", "}").unwrap()].text, "b");
        assert_eq!(depth_of(&tokens("if x {")), 1, "an OPEN block is nesting");
        assert_eq!(
            depth_of(&tokens(r#"let s = "{";"#)),
            0,
            "not a brace in a literal"
        );
    }

    /// The loop closes every turn AFTER its receive and BEFORE the
    /// flush, outside every arm.
    ///
    /// The behavioural tests cover what the operation decides; this
    /// covers the ordering they cannot, because they call `close_turn`
    /// directly and so witness no receive at all. The loop owns a
    /// terminal and cannot be driven from here, so the witness is
    /// structural — but structural about the TURN, not about spelling.
    ///
    /// Each clause is a placement that reintroduces the bug:
    ///
    /// - BEFORE the receive: an attendance change arriving during the
    ///   blocking receive is not observed until the turn after next, so
    ///   the panel repaints a dead waiter as healthy. The scanner this
    ///   replaced rejected that placement explicitly, and its own
    ///   comment recorded a regression where a misplaced probe passed
    ///   an incorrectly anchored check. The first version of THIS test
    ///   lost that clause and would have passed the same mutation
    ///   (codex on fcee1e7).
    /// - INSIDE an arm: the check becomes conditional on which input
    ///   arrived, and a dead waiter changes no file — so the data-wake
    ///   arm is exactly the one it would be skipped on.
    /// - AFTER the flush: its answer arrives one rebuild too late.
    ///
    /// Offsets and balanced delimiters only: no character windows and
    /// no indentation, so a reflow cannot switch it off and a comment
    /// cannot make it red.
    #[test]
    fn the_loop_closes_every_turn_after_its_receive_and_before_the_flush() {
        // Compiler anchors for every name the token patterns below
        // spell, the fingerprint dependency included. A rename must
        // stop this test BUILDING rather than leave it hunting a dead
        // string: that is how its predecessor was red for months while
        // guarding nothing.
        type Close =
            fn(&mut AttendanceProbe, std::time::Instant, Duration, bool, fn() -> u64) -> TurnClose;
        type Fingerprint = fn(&std::path::Path) -> u64;
        let _close: Close = close_turn;
        let _fingerprint: Fingerprint = crate::cli::status::attendance_fingerprint;

        const RECV: &[&str] = &["match", "ev_rx", ".", "recv_timeout", "(", "wait", ")", "{"];
        const CLOSE: &[&str] = &["refresh_pending", "=", "close_turn", "("];
        const FLUSH: &[&str] = &[
            "if",
            "refresh_pending",
            "&",
            "&",
            "last_rebuild",
            ".",
            "elapsed",
            "(",
            ")",
            ">",
            "=",
            "REBUILD_MIN",
            "{",
        ];
        const CARRIED: &[&str] = &[".", "refresh_pending", ";"];

        let whole = include_str!("mod.rs");
        // Production only. Without this the patterns could be
        // satisfied by the strings in this very test.
        let prod = &whole[..whole
            .find("\n#[cfg(test)]\npub(crate) mod tests {")
            .expect("the test module still opens with that attribute")];
        let toks = tokens(prod);

        // The MAIN receive. Not the detail overlay's earlier
        // `recv_timeout`, which belongs to a different loop.
        let recv = find_seq(&toks, RECV).expect("the panel's own receive");
        let arms = recv + RECV.len() - 1;
        let arms_end = pair_end(&toks, arms, "{", "}").expect("the match block closes");

        let close = find_seq(&toks, CLOSE)
            .expect("the turn's completion, assigned back to refresh_pending");
        let flush = find_seq(&toks, FLUSH).expect("the trailing refresh flush");

        assert!(
            recv < close,
            "the completion must come AFTER the receive, or a change arriving during \
             the blocking receive is not seen until the turn after next"
        );
        assert!(
            arms_end <= close,
            "and OUTSIDE every arm, or it is conditional on which input arrived"
        );
        assert!(
            close < flush,
            "and BEFORE the flush, or its answer reaches the rebuild one turn late"
        );
        // Ordering is not enough: `if refresh_pending { … }` wrapped
        // around the call also sits between the two. Depth is what
        // separates "at the turn's end" from "inside a branch there".
        assert_eq!(
            depth_of(&toks[arms_end..close]),
            0,
            "no block may open between the receive's end and the completion, or the \
             probe is conditional on which input arrived"
        );

        // The call's own arguments, by balancing its parentheses over
        // the same tokens — no character window.
        let paren = close + CLOSE.len() - 1;
        let args_end = pair_end(&toks, paren, "(", ")").expect("the call's arguments close");
        let args = &toks[paren..args_end];
        for needed in [
            &["&", "mut", "waker_probe"][..],
            &["REBUILD_MIN"][..],
            &["refresh_pending"][..],
            &[
                "crate",
                ":",
                ":",
                "cli",
                ":",
                ":",
                "status",
                ":",
                ":",
                "attendance_fingerprint",
                "(",
                "&",
                "repo",
                ")",
            ][..],
        ] {
            assert!(
                find_seq(args, needed).is_some(),
                "the loop's own {} must be what it closes the turn with",
                needed.join("")
            );
        }
        // And the answer is carried, pinned to THIS call's closing
        // paren rather than searched for between here and the flush: a
        // span search passes as long as some `.refresh_pending;`
        // exists in it, which is true today only because there happens
        // to be exactly one (ruthless on d1b9375). Nothing legitimate
        // reflows between `)` and `.refresh_pending;`, and the rewrite
        // that would — binding the result to a local first — already
        // fails the CLOSE pattern.
        let carried: Vec<&str> = toks[args_end..]
            .iter()
            .take(CARRIED.len())
            .map(|t| t.text)
            .collect();
        assert_eq!(
            carried, CARRIED,
            "close_turn's refresh decision must be read off THIS call and assigned, \
             not merely mentioned somewhere before the flush"
        );
    }

    /// A turn's CLOSE is what raises the rebuild, and these drive the
    /// production operation rather than the helper beside it.
    ///
    /// `attendance_changed` was already well covered and its call site
    /// was not: deleting the call from the loop left every test green,
    /// including the one that names the helper through the compiler
    /// (codex on fbb84e3). The decision now has a return value, so a
    /// test can assert what the turn decided instead of reading the
    /// source to see whether it asked.
    #[test]
    fn a_turn_closes_by_asking_whether_attendance_changed() {
        let every = Duration::from_secs(1);
        let t0 = std::time::Instant::now();
        let due = t0 + every;
        let probe = |fp: u64| AttendanceProbe {
            at: t0,
            fingerprint: fp,
        };

        // A due flip on an idle turn asks for the rebuild.
        let mut p = probe(7);
        let close = close_turn(&mut p, due, every, false, || 9);
        assert_eq!(
            close,
            TurnClose {
                refresh_pending: true,
                observed: 1
            },
            "a changed attendance is the one input with no event behind it"
        );

        // Unchanged attendance asks for nothing. The nothing-changed
        // gate downstream is not what should be absorbing this.
        let mut p = probe(7);
        let close = close_turn(&mut p, due, every, false, || 7);
        assert_eq!(
            close,
            TurnClose {
                refresh_pending: false,
                observed: 1
            },
            "unchanged attendance must not request a rebuild"
        );

        // A rebuild already owed by this turn's own input SURVIVES.
        // Handing `refresh_pending` through is what makes that true;
        // assigning the probe's answer over it would drop the request.
        let mut p = probe(7);
        let close = close_turn(&mut p, due, every, true, || 7);
        assert!(
            close.refresh_pending,
            "a pending rebuild cannot be cancelled by an unchanged probe"
        );

        // Not yet due: no read at all. The reads are cheap, not free,
        // and a key storm turns this loop many times.
        let mut p = probe(7);
        let close = close_turn(&mut p, t0, every, false, || {
            panic!("a probe that is not due must not read")
        });
        assert_eq!(
            close,
            TurnClose {
                refresh_pending: false,
                observed: 0
            }
        );
        let mut p = probe(7);
        assert!(
            close_turn(&mut p, t0, every, true, || 7).refresh_pending,
            "and it still carries an owed rebuild through"
        );
    }

    /// The probe is NOT conditional on this turn having had a
    /// filesystem refresh.
    ///
    /// Separated because it is the mutation that would otherwise look
    /// harmless: wrapping the check so it only runs when something
    /// already changed restores the original bug exactly — a dead
    /// waiter changes no file, so that turn is the one turn the check
    /// would never run on.
    #[test]
    fn a_turn_probes_whether_or_not_its_input_asked_for_a_rebuild() {
        let every = Duration::from_secs(1);
        let t0 = std::time::Instant::now();
        let due = t0 + every;
        for already in [false, true] {
            let mut p = AttendanceProbe {
                at: t0,
                fingerprint: 7,
            };
            let close = close_turn(&mut p, due, every, already, || 9);
            assert_eq!(
                close.observed, 1,
                "the fingerprint must be read on a turn whose refresh_pending was \
                 {already}, or a dead waiter is invisible on exactly the turns that \
                 matter"
            );
            assert!(close.refresh_pending);
        }
    }

    /// THE PANEL ASKS, on its own turns, whether a waker died.
    ///
    /// Every other input announces itself with a watcher event. This
    /// one does not: killing a waiter changes no file, so nothing
    /// raises `refresh_pending` and the loop repaints the snapshot it
    /// holds. A test that calls `input_signature` itself proves the
    /// signature CAN see it and says nothing about whether the loop
    /// ever asks — which is how the reported failure survived its own
    /// regression (grok 8aa2736120d7).
    #[test]
    fn the_panel_probes_for_a_completed_attendance_between_filesystem_events() {
        let every = Duration::from_secs(1);
        let t0 = std::time::Instant::now();
        let mut state = AttendanceProbe {
            at: t0,
            fingerprint: 7,
        };

        // Too soon: it does not even ask. The probe panics to prove
        // the rate limit is around it, not inside it.
        assert!(!attendance_changed(&mut state, t0, every, || panic!(
            "probed inside the rate limit"
        )));
        assert!(!attendance_changed(
            &mut state,
            t0 + Duration::from_millis(999),
            every,
            || panic!("probed inside the rate limit")
        ));

        // Due, and nothing changed: no rebuild is asked for.
        let t1 = t0 + every;
        assert!(!attendance_changed(&mut state, t1, every, || 7));

        // Due, and the verdict FLIPPED — a waiter died, with no file
        // touched. This is the raise the panel never made.
        let t2 = t1 + every;
        assert!(attendance_changed(&mut state, t2, every, || 9));

        // And it flips ONCE: the new verdict is now the baseline, so
        // an unchanged world does not rebuild every tick.
        let t3 = t2 + every;
        assert!(!attendance_changed(&mut state, t3, every, || 9));

        // Back again is also a change — a waiter arriving matters as
        // much as one dying.
        let t4 = t3 + every;
        assert!(attendance_changed(&mut state, t4, every, || 7));
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

    /// Every mode that holds a candidate by index says so in ONE
    /// place, so the refresh captures its label and keeps its list.
    /// The coverage step was added to the rebind and not to the
    /// capture, so an ordinary background refresh dropped the list and
    /// sent the operator back to the panel mid-choice.
    #[test]
    fn every_mode_holding_a_candidate_keeps_it_across_a_refresh() {
        use crate::cli::teams_config::ReviewKind;
        let holders = [
            Mode::AddPicker { sel: 2 },
            Mode::SwapPicker { out: 0, sel: 2 },
            Mode::AddReviews {
                candidate: 2,
                kind: ReviewKind::Commit,
                sel: 0,
            },
            Mode::Confirm {
                action: ConfirmAction::AddCandidate {
                    idx: 2,
                    tier: ReviewKind::Commit,
                },
            },
        ];
        for mode in holders {
            assert_eq!(
                candidate_index(mode),
                Some(2),
                "{mode:?} holds a candidate the refresh must capture"
            );
        }
        // And pages that hold none do not pin the list open.
        for mode in [
            Mode::LogScroll,
            Mode::AgentDetail { idx: 0, sel: 0 },
            Mode::RemotePage { sel: 0 },
        ] {
            assert_eq!(candidate_index(mode), None, "{mode:?}");
        }

        // A reorder under the coverage step re-locates the candidate
        // by LABEL, so the roster gains the agent that was chosen.
        let before = ["alpha", "beta", "gamma"];
        let after = ["gamma", "alpha", "beta"];
        let picker: Vec<crate::cli::status::AvailableAgent> = after
            .iter()
            .map(|l| crate::cli::status::AvailableAgent {
                label: (*l).to_string(),
                tool: "x".into(),
                invocation: "x".into(),
                description: None,
            })
            .collect();
        let was = before[2]; // gamma, at index 2 before the reorder
        assert_eq!(
            rebind_picker_sel_by_label(2, Some(was), &picker),
            Some(0),
            "gamma moved to the front and the choice followed it"
        );
    }

    #[test]
    fn roster_confirm_decision_returns_to_origin_pages() {
        assert_eq!(
            roster_confirm_after_decision(
                ConfirmAction::AddCandidate {
                    idx: 1,
                    tier: crate::cli::teams_config::ReviewKind::Commit
                },
                false,
                3,
                2,
                2
            ),
            (
                Mode::AddReviews {
                    candidate: 1,
                    kind: crate::cli::teams_config::ReviewKind::Commit,
                    sel: 0
                },
                false
            ),
            // Back to the step that asked WHAT they review, carrying
            // the coverage already chosen — not back to choosing WHO,
            // which the operator already answered.
            "a refused add returns to the review step, not the picker"
        );
        assert_eq!(
            roster_confirm_after_decision(ConfirmAction::RemoveAgent { idx: 1 }, false, 0, 2, 2),
            (Mode::AgentDetail { idx: 1, sel: 0 }, false),
            "cancel/failed remove returns to the target detail page"
        );
        assert_eq!(
            roster_confirm_after_decision(
                ConfirmAction::AddCandidate {
                    idx: 4,
                    tier: crate::cli::teams_config::ReviewKind::Commit
                },
                false,
                2,
                2,
                2
            ),
            (Mode::AgentPanel { sel: 2 }, true),
            "an invalid add target falls back to the panel and drops picker state"
        );
        assert_eq!(
            roster_confirm_after_decision(ConfirmAction::RemoveAgent { idx: 1 }, true, 0, 2, 2),
            (Mode::AgentPanel { sel: 2 }, true),
            "successful roster writes refresh from the panel"
        );
    }

    #[test]
    fn roster_confirm_refresh_rebinds_by_identity() {
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;

        let picker = vec![
            cand("scout", "codex", "codex"),
            cand("grok", "grok", "grok"),
        ];
        // A NON-default tier, so this proves the chosen tier survives the
        // refresh rather than merely that the call compiles.
        let chosen = crate::cli::teams_config::ReviewKind::Gate;
        assert_eq!(
            rebind_add_confirm_after_refresh(0, Some("grok"), &picker, 2, chosen),
            Mode::Confirm {
                action: ConfirmAction::AddCandidate {
                    idx: 1,
                    tier: chosen
                }
            },
            "add confirm follows the same candidate, and keeps the tier, after a reorder"
        );
        assert_eq!(
            rebind_add_confirm_after_refresh(0, Some("missing"), &picker, 2, chosen),
            Mode::AgentPanel { sel: 2 },
            "add confirm closes instead of retargeting when the candidate vanished"
        );

        let agents = vec![
            agent_row("codex", RosterRole::Commit, AutoMode::Off),
            agent_row("claude", RosterRole::Master, AutoMode::On),
        ];
        assert_eq!(
            rebind_remove_confirm_after_refresh(Some("codex"), &agents, agents.len()),
            Mode::Confirm {
                action: ConfirmAction::RemoveAgent { idx: 0 }
            },
            "remove confirm follows the same agent after roster reorder"
        );
        assert_eq!(
            rebind_remove_confirm_after_refresh(Some("missing"), &agents, agents.len()),
            Mode::AgentPanel { sel: agents.len() },
            "remove confirm closes when the target vanished"
        );
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
    fn overlay_scroll_clamps_to_the_document() {
        let mut o = Overlay::info("facts".into(), "body".into());
        o.scroll(100, 8);
        assert_eq!(o.offset, 8, "clamped to the last page");
        o.scroll(-100, 8);
        assert_eq!(o.offset, 0, "clamped at the top");
    }

    fn git_in(p: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(p)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// Active `foo`, finished `bar`, a queued entry, and a commit per
    /// kind of tag: the intro, `[foo,bar,misc,foo]`, an ad-hoc `tweak`,
    /// a plan that has no page, and names that are not plan keys — one
    /// a path that would reach the queued entry.
    fn repo_with_tagged_commits() -> tempfile::TempDir {
        let repo = tempfile::TempDir::new().unwrap();
        let p = repo.path();
        git_in(p, &["init", "--quiet", "--initial-branch=main"]);
        git_in(p, &["config", "user.email", "t@t"]);
        git_in(p, &["config", "user.name", "t"]);
        git_in(p, &["config", "commit.gpgsign", "false"]);
        let write = |rel: &str, body: &str| {
            std::fs::create_dir_all(p.join(rel).parent().unwrap()).unwrap();
            std::fs::write(p.join(rel), body).unwrap();
        };
        write(".clank/plans/foo.md", "# foo\n");
        write(".clank/finished/bar.md", "# bar\n");
        write(".clank/queue/500-queued.md", "# queued\n");
        for (i, subject) in [
            "[foo] intro",
            "[bar] finished work",
            "[foo] current work",
            "tweak",
            "[gone] a purged plan's commit",
            "[../queue/500-queued] not a plan key",
        ]
        .iter()
        .enumerate()
        {
            write(&format!("f{i}.rs"), subject);
            git_in(p, &["add", "-A"]);
            git_in(p, &["commit", "--quiet", "-m", subject]);
        }
        repo
    }

    fn sha_of(p: &std::path::Path, subject: &str) -> crate::lifecycle::CommitSha {
        let log = git_in(p, &["log", "--format=%H %s"]);
        let (sha, _) = log
            .lines()
            .filter_map(|l| l.split_once(' '))
            .find(|(_, s)| *s == subject)
            .unwrap_or_else(|| panic!("no commit `{subject}`"));
        crate::lifecycle::CommitSha::parse(sha).unwrap()
    }

    fn key(s: &str) -> crate::lifecycle::PlanKey {
        crate::lifecycle::PlanKey::parse(s).unwrap()
    }

    /// A commit page offers its tagged active or finished plan when readable.
    #[test]
    fn a_commit_page_offers_its_one_readable_tagged_plan() {
        use crate::cli::status_tui::fixtures::{plan_state, snap};
        use clank_core::plan_view::WaitingOn;
        let repo = repo_with_tagged_commits();
        let p = repo.path();
        let s = snap(Some(plan_state("foo", WaitingOn::MasterToContinue)), vec![]);
        let rows = |subject: &str| {
            let (cp, mode) = open_commit_page(p, &sha_of(p, subject), None, &s, 80).unwrap();
            assert_eq!(mode, Mode::CommitPage { sel: 0 }, "opens on its rows");
            commit_actions(&cp.plan)
        };
        use CommitAction::*;
        assert_eq!(
            rows("[bar] finished work"),
            [OpenHtml, OpenPlan(key("bar")), Back],
            "a finished plan has a page"
        );
        assert_eq!(
            rows("[foo] current work"),
            [OpenHtml, OpenPlan(key("foo")), Back]
        );
        assert_eq!(rows("tweak"), [OpenHtml, Back], "an ad-hoc commit");
        assert_eq!(
            rows("[gone] a purged plan's commit"),
            [OpenHtml, Back],
            "a tag naming no plan"
        );
        assert_eq!(
            rows("[../queue/500-queued] not a plan key"),
            [OpenHtml, Back],
            "a name that is not a plan key is not a path to read"
        );
    }

    /// A plan opened from a commit page returns to it, at its scroll
    /// and on the row that opened it — whatever the cursor — through
    /// Back and through the refresh that finds the plan gone, which
    /// returns to no row; with no commit page beneath, to the log.
    #[tokio::test]
    async fn a_plan_opened_from_a_commit_page_returns_to_it() {
        use crate::cli::status_tui::fixtures::{plan_state, snap};
        use clank_core::plan_view::WaitingOn;
        let repo = repo_with_tagged_commits();
        let p = repo.path();
        let s = snap(Some(plan_state("foo", WaitingOn::MasterToContinue)), vec![]);
        let finished = sha_of(p, "[bar] finished work");
        let (mut cp, _) = open_commit_page(p, &finished, None, &s, 80).unwrap();
        cp.scroll = 3;
        let (mut commit_page, mut beneath) = (Some(cp), None);

        let mut plan_page =
            open_plan_from_commit(p, &mut commit_page, &mut beneath, &key("bar"), &s).await;
        assert!(
            matches!(
                plan_page.as_ref().map(|pp| pp.st),
                Some(input::PlanPageState::Finished { .. })
            ),
            "{plan_page:?}"
        );
        assert!(commit_page.is_none(), "the commit page waits beneath");
        let back = close_plan_page(p, &mut plan_page, &mut beneath, &mut commit_page, &s);
        assert_eq!(back, Mode::CommitPage { sel: 1 }, "on bar's row");
        assert!(plan_page.is_none() && beneath.is_none());
        assert_eq!(commit_page.as_ref().unwrap().scroll, 3, "at its scroll");

        let mut plan_page =
            open_plan_from_commit(p, &mut commit_page, &mut beneath, &key("bar"), &s).await;
        std::fs::remove_file(p.join(".clank/finished/bar.md")).unwrap();
        let closed = refetch_or_close(p, &mut plan_page, &mut beneath, &mut commit_page, &s)
            .await
            .expect_err("bar's page closes");
        let cp = commit_page.as_ref().unwrap();
        assert_eq!(cp.plan, None, "bar has no page to offer now");
        assert_eq!(
            closed,
            Mode::CommitPage {
                sel: document_focus(&commit_actions(&cp.plan))
            },
            "its row is gone, so the cursor is on none"
        );

        let mut plan_page = open_plan_page(p, &key("foo"), &s).await;
        assert!(plan_page.is_some());
        assert_eq!(
            close_plan_page(p, &mut plan_page, &mut None, &mut None, &s),
            Mode::LogScroll
        );
    }

    /// A review row opens the page with the document focused on that
    /// reviewer's feedback, as the overlay it replaces did.
    #[test]
    fn a_review_row_opens_the_commit_page_at_its_feedback() {
        use crate::cli::status_tui::fixtures::snap;
        let repo = repo_with_tagged_commits();
        let p = repo.path();
        let sha = sha_of(p, "[bar] finished work");
        for (author, verdict) in [
            ("codex", "REQUEST_CHANGES fix it"),
            ("ruthless", "CONTINUE lgtm"),
        ] {
            let label = clank_core::ids::AgentLabel::parse(author).unwrap();
            let rel =
                crate::disk_format::feedback_path_wire(&label, &sha, std::slice::from_ref(&sha));
            std::fs::create_dir_all(p.join(&rel).parent().unwrap()).unwrap();
            std::fs::write(
                p.join(&rel),
                format!("{verdict}\n\nthe details of {author}\n"),
            )
            .unwrap();
        }
        let s = snap(None, vec![]);
        let (cp, mode) = open_commit_page(p, &sha, Some("ruthless"), &s, 80).unwrap();
        let actions = commit_actions(&cp.plan);
        assert_eq!(
            mode,
            Mode::CommitPage {
                sel: document_focus(&actions)
            }
        );
        let layout = build_commit_lines(&commit_doc(&cp.detail), 80);
        assert_eq!(cp.scroll, layout.review_line["ruthless"]);
        assert!(
            cp.scroll > layout.review_line["codex"],
            "not the first review"
        );

        let (lines, _) = render_commit_page(&cp, &actions, document_focus(&actions), 5, 80);
        assert!(
            crate::cli::status_tui::fixtures::visible(&lines[1]).contains("ruthless"),
            "the reviewer's block heads the document: {lines:?}"
        );
    }

    #[test]
    fn panel_focus_forces_the_log_to_its_top() {
        // THE INVARIANT: panel focus ⟹ log topped. A wheel
        // burst used to cross into the panel mid-batch, freezing a
        // mid-scroll viewport under the focused panel; Tab could do the
        // same from any depth. The loop-pass enforcement makes the state
        // unrepresentable regardless of which path entered the panel.
        let mut log = LogView::new();
        log.cursor = 7;
        log.offset = 5;
        enforce_panel_tops_log(Mode::AgentPanel { sel: 0 }, &mut log);
        assert_eq!((log.cursor, log.offset), (0, 0), "panel focus tops the log");

        // The log, and the pages its entries open, keep its viewport
        // for Back to return to.
        for mode in [
            Mode::LogScroll,
            Mode::CommitPage { sel: 0 },
            Mode::EventDetail { sel: 0 },
            Mode::PlanDetail { sel: 0 },
        ] {
            let mut log = LogView::new();
            log.cursor = 7;
            log.offset = 5;
            enforce_panel_tops_log(mode, &mut log);
            assert_eq!((log.cursor, log.offset), (7, 5), "{mode:?}");
        }
    }

    fn numbered_commit(i: usize) -> crate::cli::log::OnelineRow {
        crate::cli::log::OnelineRow::Commit {
            at: 0,
            sha: crate::lifecycle::CommitSha::parse(&format!("{:040x}", i + 1)).unwrap(),
            subject: format!("subject {i}"),
            marker: crate::cli::log::RowMarker::Plain,
            stands_for: None,
        }
    }

    fn long_log_snap(n: usize) -> StatusSnapshot {
        let mut s = two_agent_snap();
        s.log_rows = (0..n).map(numbered_commit).collect();
        s
    }

    fn log_frame(mode: Mode) -> PanelView<'static> {
        PanelView {
            source_page: None,
            source_group: None,
            plan_page: None,
            commit_page: None,
            event_page: None,
            wait_page: None,
            wait_process: None,
            wait_scroll: 0,
            seen_session: None,
            clear_target: None,
            plan_input: None,
            block_page: None,
            mode,
            picker: &[],
            log_cursor: 0,
            reach: crate::cli::status_tui::zellij::ZellijReach::NotInSession,
            remote: Default::default(),
            remote_detail: None,
            remote_page: None,
            presence: None,
            lost: Default::default(),
            lift: 0,
        }
    }

    /// One pass of the loop's paint: settle, then render what it drew.
    fn paint_log(log: &mut LogView, s: &StatusSnapshot, mode: Mode, rows: u16) -> Vec<String> {
        let view = settle_frame(log, s, log_frame(mode), rows, 60, 0, 0);
        render_at(s, rows, 60, log.offset, 0, &view)
            .0
            .iter()
            .map(|l| visible(l))
            .collect()
    }

    fn selected_subject(log: &LogView, s: &StatusSnapshot) -> String {
        match &s.log_rows[log.cursor] {
            crate::cli::log::OnelineRow::Commit { subject, .. } => subject.clone(),
            other => panic!("cursor on {other:?}"),
        }
    }

    fn shows(screen: &[String], subject: &str) -> bool {
        screen.iter().any(|l| l.contains(subject))
    }

    #[test]
    fn the_log_takes_the_pane_as_its_cursor_descends_and_gives_it_back() {
        let s = long_log_snap(30);
        let rule = |i: usize| is_rule(&s, 0, i);
        let mut log = LogView::new();
        let top = paint_log(&mut log, &s, Mode::LogScroll, 12);
        assert!(shows(&top, "add agent"), "{top:#?}");
        let mut lifts = vec![];
        for _ in 0..20 {
            log.down(30, &rule);
            let screen = paint_log(&mut log, &s, Mode::LogScroll, 12);
            assert!(shows(&screen, &selected_subject(&log, &s)), "{screen:#?}");
            lifts.push(settle_frame(&mut log, &s, log_frame(Mode::LogScroll), 12, 60, 0, 0).lift);
        }
        assert!(lifts.windows(2).all(|w| w[0] <= w[1]), "{lifts:?}");
        let deep = paint_log(&mut log, &s, Mode::LogScroll, 12);
        assert!(!shows(&deep, "add agent"), "{deep:#?}");
        let entries = deep.iter().filter(|l| l.contains("subject ")).count();
        assert_eq!(
            entries,
            12 - 2,
            "every row but the bar and the log's rule: {deep:#?}"
        );
        for _ in 0..20 {
            log.up(30, &rule);
            let screen = paint_log(&mut log, &s, Mode::LogScroll, 12);
            assert!(shows(&screen, &selected_subject(&log, &s)), "{screen:#?}");
        }
        assert_eq!(paint_log(&mut log, &s, Mode::LogScroll, 12), top);
    }

    #[test]
    fn leaving_the_log_for_the_panel_gives_the_header_back() {
        let s = long_log_snap(30);
        let mut log = LogView::new();
        let panel = Mode::AgentPanel { sel: 0 };
        let fresh = paint_log(&mut log, &s, panel, 12);
        log.page_down(20, 30, &|_| false);
        paint_log(&mut log, &s, Mode::LogScroll, 12);
        enforce_panel_tops_log(panel, &mut log);
        assert_eq!(paint_log(&mut log, &s, panel, 12), fresh);
    }

    #[test]
    fn a_log_that_fits_with_its_header_never_lifts() {
        let s = long_log_snap(4);
        let mut log = LogView::new();
        let top = paint_log(&mut log, &s, Mode::LogScroll, 30);
        for _ in 0..4 {
            log.down(4, &|_| false);
            let screen = paint_log(&mut log, &s, Mode::LogScroll, 30);
            assert_eq!(screen.len(), top.len());
            assert!(shows(&screen, "add agent"), "{screen:#?}");
            assert!((0..4).all(|i| shows(&screen, &format!("subject {i}"))));
        }
    }

    #[test]
    fn a_resize_while_expanded_keeps_the_selection_on_screen() {
        let s = long_log_snap(40);
        let mut log = LogView::new();
        log.page_down(25, 40, &|_| false);
        paint_log(&mut log, &s, Mode::LogScroll, 30);
        let held = selected_subject(&log, &s);
        for rows in [6u16, 3, 2, 40, 12] {
            let screen = paint_log(&mut log, &s, Mode::LogScroll, rows);
            assert_eq!(selected_subject(&log, &s), held);
            assert!(shows(&screen, &held), "rows {rows}: {screen:#?}");
        }
    }

    #[test]
    fn a_refresh_with_a_new_leading_event_keeps_the_selected_entry() {
        let mut s = long_log_snap(30);
        let mut log = LogView::new();
        log.page_down(15, 30, &|_| false);
        paint_log(&mut log, &s, Mode::LogScroll, 12);
        for _ in 0..4 {
            log.up(30, &|_| false);
        }
        let before = paint_log(&mut log, &s, Mode::LogScroll, 12);
        let held_subject = selected_subject(&log, &s);
        assert!(log.cursor > log.offset, "premise: not at an edge");
        let row = log.cursor - log.offset;
        let held = held_entry(Mode::LogScroll, &s, 60, &log);
        s.log_rows.insert(0, numbered_commit(100));
        s.log_rows.insert(0, numbered_commit(101));
        refind_entry(&mut log, held, &s, 60);
        let after = paint_log(&mut log, &s, Mode::LogScroll, 12);
        assert_eq!(selected_subject(&log, &s), held_subject);
        assert_eq!(log.cursor - log.offset, row, "the same screen row");
        assert_eq!(after, before, "nothing in view moved");
    }

    /// The rows an event's index points into are rebuilt by every
    /// refresh, so the event is found by its keys.
    #[test]
    fn a_refresh_keeps_a_selected_event_whose_index_moved() {
        use crate::cli::github::log::{EventLog, Transport};
        use crate::cli::github::timeline;
        let repo = tempfile::tempdir().unwrap();
        let dir = repo.path().join(".clank/agents/bob/events");
        std::fs::create_dir_all(&dir).unwrap();
        let mut events = EventLog::open(&dir, "github-o-r-aaaa").unwrap();
        let item = |number: u64| clank_core::wait::WaitItem::GithubEvent {
            presentation: None,
            repo: "o/r".into(),
            event: "pr_comment".into(),
            detail: None,
            number: Some(number),
            title: Some(format!("event {number}")),
            actor: Some("alice".into()),
            url: None,
            instructions: None,
            content: None,
        };
        let read = |s: &mut StatusSnapshot| {
            let read = timeline::timeline_snapshot(repo.path());
            let items = crate::cli::log::interleave(&[], &read.events);
            s.log_rows = crate::cli::log::oneline_items_rows(
                &items,
                &Default::default(),
                &Default::default(),
            );
            s.github_events = read.events;
        };
        for n in 1..=3 {
            events
                .append_inbox(
                    Transport::Poll,
                    Some(&format!("f{n}")),
                    None,
                    Some(n * 1_000),
                    &item(n),
                )
                .unwrap();
        }
        let mut s = two_agent_snap();
        read(&mut s);
        let selected = |log: &LogView, s: &StatusSnapshot| match &s.log_rows[log.cursor] {
            crate::cli::log::OnelineRow::Github {
                event_idx, line, ..
            } => (s.github_events[*event_idx].page_keys(), line.clone()),
            other => panic!("{other:?}"),
        };
        let mut log = LogView::new();
        log.down(3, &|_| false);
        let (before, _) = selected(&log, &s);
        let held = held_entry(Mode::LogScroll, &s, 60, &log);
        events
            .append_inbox(Transport::Poll, Some("f4"), None, Some(4_000), &item(4))
            .unwrap();
        // A second holder's copy joins the selected event to a key it
        // did not have.
        let dir = repo.path().join(".clank/agents/alice/events");
        std::fs::create_dir_all(&dir).unwrap();
        EventLog::open(&dir, "github-o-r-aaaa")
            .unwrap()
            .append_inbox(Transport::Poll, Some("f2"), None, Some(2_000), &item(2))
            .unwrap();
        read(&mut s);
        refind_entry(&mut log, held, &s, 60);
        let (after, line) = selected(&log, &s);
        assert!(line.contains("event 2"), "{line}");
        assert!(after.len() > before.len() && before.iter().all(|k| after.contains(k)));
        assert_eq!(log.cursor, 2, "one newer event above it");
    }

    /// The paint highlights the cursor settle clamped, not the one the
    /// frame's view was built with.
    #[test]
    fn the_painted_cursor_is_the_settled_one() {
        let s = long_log_snap(30);
        let mut log = LogView::new();
        log.cursor = 50;
        let view = settle_frame(&mut log, &s, log_frame(Mode::LogScroll), 12, 60, 0, 0);
        assert_eq!((view.log_cursor, log.cursor), (29, 29));
    }

    #[test]
    fn an_authors_verdicts_are_told_apart_by_the_commit_they_review() {
        use crate::cli::log::OnelineRow;
        let review = || OnelineRow::Review {
            verdict: clank_core::vocab::Verdict::Continue,
            author: "codex".into(),
            summary: "ok".into(),
            at: None,
        };
        let mut s = two_agent_snap();
        s.log_rows = vec![review(), numbered_commit(2), review(), numbered_commit(1)];
        let mut log = LogView::new();
        log.cursor = 2;
        let held = held_entry(Mode::LogScroll, &s, 60, &log);
        s.log_rows.splice(0..0, [review(), numbered_commit(3)]);
        refind_entry(&mut log, held, &s, 60);
        assert_eq!(log.cursor, 4, "not the review of 2, now where it sat");
    }

    /// Partway down, with part of the header lifted: what is in view
    /// stays in view, header included, and the selection sits on the
    /// same terminal row.
    fn partly_lifted(s: &StatusSnapshot) -> (LogView, Vec<String>) {
        let mut log = LogView::new();
        let mut screen = paint_log(&mut log, s, Mode::LogScroll, 12);
        for _ in 0..s.log_rows.len() {
            if log.lift >= 2 {
                break;
            }
            log.down(s.log_rows.len(), &|_| false);
            screen = paint_log(&mut log, s, Mode::LogScroll, 12);
        }
        assert!(log.lift >= 2, "premise: the header lifts partway");
        assert!(
            shows(&screen, "add agent"),
            "premise: header partly shown {screen:#?}"
        );
        (log, screen)
    }

    #[test]
    fn a_refresh_partway_down_moves_nothing_on_screen() {
        let mut s = long_log_snap(40);
        let (mut log, before) = partly_lifted(&s);
        let held = held_entry(Mode::LogScroll, &s, 60, &log);
        for n in 100..103 {
            s.log_rows.insert(0, numbered_commit(n));
        }
        refind_entry(&mut log, held, &s, 60);
        assert_eq!(paint_log(&mut log, &s, Mode::LogScroll, 12), before);
        let (offset, lift) = (log.offset, log.lift);
        let mut deeper = log.clone();
        while deeper.lift == lift && deeper.cursor < 40 {
            deeper.down(s.log_rows.len(), &|_| false);
            paint_log(&mut deeper, &s, Mode::LogScroll, 12);
        }
        assert_eq!(
            (deeper.offset, deeper.lift),
            (offset, lift + 1),
            "the header goes before the log scrolls"
        );
        let mut fresh = LogView::new();
        let top = paint_log(&mut fresh, &s, Mode::LogScroll, 12);
        for _ in 0..40 {
            log.up(s.log_rows.len(), &|_| false);
            paint_log(&mut log, &s, Mode::LogScroll, 12);
        }
        assert_eq!(
            paint_log(&mut log, &s, Mode::LogScroll, 12),
            top,
            "the top of the log brings the whole header back"
        );
    }

    #[test]
    fn a_refresh_under_a_page_partway_down_returns_to_the_same_frame() {
        let mut s = long_log_snap(40);
        let (mut log, before) = partly_lifted(&s);
        let page = Mode::CommitPage { sel: 0 };
        paint_log(&mut log, &s, page, 12);
        assert!(log.lift >= 2, "the page leaves the log's lift alone");
        let held = held_entry(page, &s, 60, &log);
        s.log_rows.insert(0, numbered_commit(100));
        refind_entry(&mut log, held, &s, 60);
        paint_log(&mut log, &s, page, 12);
        assert_eq!(paint_log(&mut log, &s, Mode::LogScroll, 12), before);
    }

    #[test]
    fn a_header_that_grows_under_a_refresh_keeps_the_selection_in_view() {
        let mut s = long_log_snap(40);
        let (mut log, _) = partly_lifted(&s);
        let selected = selected_subject(&log, &s);
        let held = held_entry(Mode::LogScroll, &s, 60, &log);
        s.log_rows.insert(0, numbered_commit(100));
        for i in 0..6 {
            s.agents.push(agent_row(
                &format!("extra{i}"),
                crate::cli::teams_config::RosterRole::Commit,
                clank_core::vocab::AutoMode::On,
            ));
        }
        refind_entry(&mut log, held, &s, 60);
        let screen = paint_log(&mut log, &s, Mode::LogScroll, 12);
        assert_eq!(selected_subject(&log, &s), selected);
        assert!(shows(&screen, &selected), "{screen:#?}");
        assert_eq!(screen.len(), 12);
    }

    #[test]
    fn a_recurring_entry_is_found_where_it_was_selected() {
        use crate::cli::log::OnelineRow;
        let notice = || OnelineRow::Notice("log unreadable".into());
        let mut s = two_agent_snap();
        s.log_rows = vec![
            notice(),
            numbered_commit(1),
            notice(),
            numbered_commit(2),
            notice(),
        ];
        let mut log = LogView::new();
        log.cursor = 2;
        let held = held_entry(Mode::LogScroll, &s, 60, &log);
        // The refresh reads the same window: a row in at the top pushes
        // one out at the bottom.
        s.log_rows.insert(0, numbered_commit(3));
        s.log_rows.pop();
        refind_entry(&mut log, held, &s, 60);
        assert_eq!(log.cursor, 3);
    }

    #[test]
    fn a_page_opened_from_the_log_returns_to_its_entry_across_a_refresh() {
        let mut s = long_log_snap(30);
        let mut log = LogView::new();
        log.page_down(15, 30, &|_| false);
        let before = paint_log(&mut log, &s, Mode::LogScroll, 12);
        let page = Mode::CommitPage { sel: 0 };
        enforce_panel_tops_log(page, &mut log);
        let held = held_entry(page, &s, 60, &log);
        s.log_rows.insert(0, numbered_commit(100));
        refind_entry(&mut log, held, &s, 60);
        enforce_panel_tops_log(page, &mut log);
        assert_eq!(paint_log(&mut log, &s, Mode::LogScroll, 12), before);
    }

    #[test]
    fn a_batch_of_moves_then_enter_opens_the_entry_it_landed_on() {
        let mut s = long_log_snap(12);
        s.log_rows.insert(3, crate::cli::log::OnelineRow::RunEnd);
        let rule = |i: usize| is_rule(&s, 0, i);
        let mut log = LogView::new();
        paint_log(&mut log, &s, Mode::LogScroll, 8);
        for _ in 0..3 {
            log.down(13, &rule);
        }
        let ask = block_ask_spans(&s, 60);
        let seq = build_scroll(&s, &ask);
        let opened = match entry_target(&seq, log.cursor) {
            Some(EntryTarget::Commit { sha, .. }) => sha,
            _ => panic!("not a commit"),
        };
        let screen = paint_log(&mut log, &s, Mode::LogScroll, 8);
        let crate::cli::log::OnelineRow::Commit { sha, subject, .. } = &s.log_rows[log.cursor]
        else {
            panic!("the rule is never a position");
        };
        assert_eq!(&opened, sha);
        assert!(shows(&screen, subject));
    }

    fn command_source(agent: &str, name: &str) -> crate::cli::source_status::ConfiguredSource {
        crate::cli::source_status::ConfiguredSource::of(
            &clank_core::ids::AgentLabel::parse(agent).unwrap(),
            &clank_core::agent_config::WaitEventSource::Command(
                clank_core::agent_config::CommandSource {
                    interrupt: false,
                    interrupt_message: None,
                    name: name.into(),
                    command: vec!["true".into()],
                    reply_command: None,
                    receipt_command: None,
                    glyph: None,
                },
            ),
        )
    }

    fn timer_source(
        name: &str,
        state: crate::cli::timer_source::TimerState,
    ) -> crate::cli::source_status::ConfiguredSource {
        let mut source = crate::cli::source_status::ConfiguredSource::of(
            &clank_core::ids::AgentLabel::parse("assistent").unwrap(),
            &clank_core::agent_config::WaitEventSource::Timer(
                clank_core::agent_config::TimerSource {
                    interrupt: false,
                    interrupt_message: None,
                    name: name.into(),
                    repeat_every: None,
                    due_at: "2026-01-01T08:00:00Z".into(),
                    content: None,
                },
            ),
        );
        source.timer = Some(state);
        source
    }

    /// Enter on the timers row lists them; a timer's page goes Back to the
    /// list on that timer, and the list goes Back to the timers row.
    #[test]
    fn a_timers_page_returns_to_the_timer_and_the_list_to_its_row() {
        use crate::cli::status_tui::input::{PanelAction, PanelRow, agent_panel_action};
        use crate::cli::timer_source::TimerState::Due;
        let mut s = two_agent_snap();
        s.sources = vec![
            command_source("alpha", "voice"),
            timer_source("later", Due(2_000)),
            timer_source("sooner", Due(1_000)),
        ];
        let rows = panel_row_list(&s, 80);
        let at = row_position(&rows, PanelRow::Timers);
        assert_eq!(rows.last(), Some(&PanelRow::Timers), "under the sources");
        assert_eq!(
            agent_panel_action(at, &rows, crate::cli::status_tui::input::Key::Enter),
            PanelAction::OpenTimers
        );
        let later = s.sources[1].clone();
        assert_eq!(
            source_back(&later, None, &s, 80),
            Mode::TimersList { sel: 1 },
            "after the sooner one"
        );
        assert_eq!(rows[at], PanelRow::Timers);
    }

    /// The list's cursor follows its timer when a refresh reorders the
    /// list: a timer due earlier arrives above it.
    #[test]
    fn the_timer_list_keeps_its_timer_across_a_refresh() {
        use crate::cli::timer_source::TimerState::Due;
        let mut s = two_agent_snap();
        s.sources = vec![timer_source("a", Due(2_000)), timer_source("b", Due(3_000))];
        let held = selected_timer(Mode::TimersList { sel: 1 }, &s);
        s.sources.push(timer_source("c", Due(1_000)));
        let timers = input::active_timers(&s.sources);
        assert_eq!(input::rebind_list_sel(1, held.as_ref(), &timers), 2);
    }

    /// An idle pane wakes when the soonest scheduled timer comes due, and
    /// not for one already owed or with no time.
    #[test]
    fn an_idle_pane_wakes_when_a_timer_comes_due() {
        use crate::cli::timer_source::TimerState::{AwaitingAck, Due};
        let mut s = two_agent_snap();
        s.sources = vec![
            timer_source("owed", Due(500)),
            timer_source("next", Due(1_100)),
            timer_source("later", Due(5_000)),
            timer_source("acking", AwaitingAck),
        ];
        assert_eq!(until_next_due(&s, 1_000), Some(Duration::from_secs(101)));
        s.sources.truncate(1);
        assert_eq!(until_next_due(&s, 1_000), None);
    }

    /// Enter on a source only one agent configures opens its page; on one
    /// several do, their list, whose Back returns to the panel's row and
    /// whose pages' Back returns to the list, on the agent left.
    #[test]
    fn a_source_row_opens_its_page_or_its_agents() {
        let mut s = two_agent_snap();
        s.sources = vec![
            command_source("alpha", "signal"),
            command_source("alpha", "voice"),
            command_source("beta", "voice"),
        ];
        let Some((Some(page), None, Mode::SourcePage { .. })) = open_source_row(0, &s.sources)
        else {
            panic!("one agent: its page");
        };
        assert_eq!(page, input::SourceRef::of(&s.sources[0]));
        let Some((None, Some(group), Mode::SourceMembers { sel: 0 })) =
            open_source_row(1, &s.sources)
        else {
            panic!("several: their list");
        };
        assert_eq!(group.members(&s.sources).len(), 2);
        assert_eq!(
            source_back(&s.sources[2], Some(&group), &s, 80),
            Mode::SourceMembers { sel: 1 }
        );
        let rows = panel_row_list(&s, 80);
        assert_eq!(
            source_row_in_panel(&group, &s, 80),
            Mode::AgentPanel {
                sel: row_position(&rows, input::PanelRow::Source(1))
            }
        );
        assert_eq!(
            source_back(&s.sources[0], None, &s, 80),
            Mode::AgentPanel {
                sel: row_position(&rows, input::PanelRow::Source(0))
            }
        );
    }

    /// The agent list's cursor follows its agent through a refresh that
    /// adds one above it.
    #[test]
    fn the_source_agent_list_keeps_its_agent_across_a_refresh() {
        let mut s = two_agent_snap();
        s.sources = vec![
            command_source("beta", "voice"),
            command_source("gamma", "voice"),
        ];
        let group = input::GroupRef::of(&s.sources[0]);
        let held = selected_member(Mode::SourceMembers { sel: 1 }, Some(&group), &s);
        s.sources.insert(0, command_source("alpha", "voice"));
        let members = group.members(&s.sources);
        assert_eq!(input::rebind_list_sel(1, held.as_ref(), &members), 2);
    }

    /// The page's facts come from the source it opened on, never the
    /// other: a queue entry and an active plan may share a stem
    /// (a-queued-plan-has-a-page).
    /// Before a refresh the loop takes what the source page's cursor is
    /// on from that page's own rows; elsewhere there is nothing to keep.
    #[test]
    fn the_source_page_cursor_is_taken_from_its_rows() {
        let mut s = fixtures::snap(None, vec![]);
        let source = crate::cli::source_status::ConfiguredSource::of(
            &clank_core::ids::AgentLabel::parse("alpha").unwrap(),
            &clank_core::agent_config::WaitEventSource::Command(
                clank_core::agent_config::CommandSource {
                    interrupt: false,
                    interrupt_message: None,
                    name: "watch".into(),
                    command: vec!["true".into()],
                    reply_command: None,
                    receipt_command: None,
                    glyph: None,
                },
            ),
        );
        s.sources = vec![source.clone()];
        let page = input::SourceRef::of(&source);
        assert_eq!(selected_source_row(Mode::LogScroll, Some(&page), &s), None);
        assert_eq!(
            selected_source_row(Mode::SourcePage { sel: 0, lift: 0 }, Some(&page), &s),
            None,
            "no rows: no monitor hosts it and it has no events"
        );
        assert_eq!(source_rows_in(&source, &s), vec![]);
        s.github_events = vec![crate::cli::github::timeline::MergedEvent {
            replies: Vec::new(),
            reply_history_pruned: false,
            outgoing: Vec::new(),
            recurrence: None,
            at: 1,
            backend: crate::cli::github::timeline::Backend::Command,
            glyph: None,
            source: "watch".into(),
            kind: "k".into(),
            failure: false,
            detail: None,
            number: None,
            content: None,
            body: None,
            actor: None,
            url: None,
            seen_by: Vec::new(),
            sender: None,
            unhandled: false,
            baseline: false,
            members: vec![crate::cli::github::timeline::MemberRef {
                woken_at: None,
                key: crate::cli::github::timeline::MemberKey {
                    agent: "alpha".into(),
                    source: source.key.clone(),
                    seq: 1,
                    ident: String::new(),
                },
                acked: true,
                transport: crate::cli::github::log::Transport::Command,
                content: None,
            }],
        }];
        assert_eq!(
            selected_source_row(Mode::SourcePage { sel: 0, lift: 0 }, Some(&page), &s),
            s.github_events[0]
                .page_keys()
                .into_iter()
                .next()
                .map(input::SourcePick::Event)
        );
    }

    /// A restart asks for this page's source with the configuration its
    /// confirm was shown; without that there is nothing safe to ask.
    #[test]
    fn a_restart_asks_for_what_the_confirm_showed() {
        let source = input::SourceRef {
            agent: "alpha".into(),
            key: "command-q".into(),
        };
        assert_eq!(
            restart_request(Some(&source), Some("shown".into())),
            Some(crate::cli::monitor::Control::Restart {
                author: "alpha".into(),
                key: "command-q".into(),
                accepted: "shown".into(),
            })
        );
        assert_eq!(
            restart_request(Some(&source), None),
            None,
            "nothing hosted was shown"
        );
        assert_eq!(restart_request(None, Some("shown".into())), None);
    }

    /// A draft's page reads its own file, follows it into the queue when
    /// queued from the page, and its Back returns to the drafts list on
    /// the row it came from, or the last one left; a draft gone from its
    /// source closes the page the same way.
    #[tokio::test]
    async fn a_draft_page_follows_its_draft() {
        use crate::cli::status_tui::fixtures::snap;
        let repo = tempfile::TempDir::new().unwrap();
        let p = repo.path();
        std::fs::create_dir_all(p.join(".clank/drafts")).unwrap();
        for name in ["idea", "other"] {
            std::fs::write(
                p.join(format!(".clank/drafts/{name}.md")),
                format!("# {name}\n\nbody of {name}\n"),
            )
            .unwrap();
        }
        let mut s = snap(None, vec![]);
        let refresh = |s: &mut StatusSnapshot| {
            s.drafts = crate::cli::queue::scan_drafts(p);
            s.queue = crate::cli::queue::scan_queue(p)
                .into_iter()
                .map(|e| crate::cli::status::QueueItemView {
                    priority: e.priority,
                    name: e.name,
                    path: e.path,
                })
                .collect();
        };
        refresh(&mut s);
        let row = s.drafts.iter().position(|d| d.name == "idea").unwrap();
        let mut page = Some(draft_page(&s.drafts[row], row));
        assert_eq!(
            selected_draft(Mode::DraftsList { sel: row }, &s),
            Some(s.drafts[row].path.clone())
        );
        assert_eq!(selected_draft(Mode::LogScroll, &s), None);
        assert!(refetch_plan_page(p, &mut page, &s).await);
        let open = page.as_ref().unwrap();
        assert_eq!(open.st, input::PlanPageState::Draft);
        assert!(open.body.as_deref().unwrap().contains("body of idea"));

        let outcome = run_plan_confirm(ConfirmAction::QueueDraft, p, "idea").await;
        assert!(outcome.is_ok(), "{outcome:?}");
        settle_plan_confirm(p, ConfirmAction::QueueDraft, &outcome, &mut page);
        refresh(&mut s);
        assert!(refetch_plan_page(p, &mut page, &s).await);
        assert_eq!(
            page.as_ref().unwrap().st,
            input::PlanPageState::Queued {
                priority: crate::cli::queue::ADD_PRIORITY
            }
        );
        assert_eq!(
            close_plan_page(p, &mut page, &mut None, &mut None, &s),
            Mode::DraftsList { sel: 0 },
            "one draft is left"
        );
        assert!(page.is_none());

        let mut gone = Some(draft_page(&s.drafts[0], 0));
        std::fs::remove_file(&s.drafts[0].path).unwrap();
        refresh(&mut s);
        assert!(!refetch_plan_page(p, &mut gone, &s).await);
        assert_eq!(
            close_plan_page(
                p,
                &mut Some(draft_page(
                    &crate::cli::queue::Draft {
                        name: "x".into(),
                        path: p.join("x.md"),
                        title: None,
                        modified: None,
                    },
                    3
                )),
                &mut None,
                &mut None,
                &s
            ),
            Mode::DraftsList { sel: 0 },
            "an empty list still takes its Back"
        );
    }

    #[tokio::test]
    async fn plan_page_facts_reads_the_source_the_page_opened_on() {
        use crate::cli::status_tui::fixtures::{plan_state, snap};
        use clank_core::plan_view::WaitingOn;
        let repo = tempfile::TempDir::new().unwrap();
        let p = repo.path();
        std::fs::create_dir_all(p.join(".clank/plans")).unwrap();
        std::fs::write(p.join(".clank/plans/foo.md"), "# foo active").unwrap();
        // The SAME stem in the queue, and only-queued `bar`.
        let mut s = snap(
            Some(plan_state("foo", WaitingOn::MasterToContinue)),
            vec!["foo", "bar"],
        );
        s.queue[0].priority = 120;
        let foo_entry = s.queue[0].path.clone();

        // Opened from the queue: queued facts, whatever the plans say.
        assert_eq!(
            plan_page_facts(p, "foo", &s, PlanSource::Queue(&foo_entry)).await,
            Some((
                input::PlanPageState::Queued { priority: 120 },
                Some(foo_entry.clone())
            ))
        );
        // Opened from the plans: active facts, whatever the queue says.
        assert_eq!(
            plan_page_facts(p, "foo", &s, PlanSource::Plans).await,
            Some((input::PlanPageState::Active { repo_paused: false }, None))
        );
        // Gone from ITS source closes the page, even if the other
        // source has the stem: `bar` promoted from a shell.
        assert_eq!(
            plan_page_facts(p, "bar", &s, PlanSource::Plans).await,
            None,
            "bar is queued, not a plan"
        );
        s.queue.retain(|q| q.name != "foo");
        assert_eq!(
            plan_page_facts(p, "foo", &s, PlanSource::Queue(&foo_entry)).await,
            None,
            "the queued page closes though an active foo exists"
        );
        assert!(
            plan_page_facts(p, "foo", &s, PlanSource::Plans)
                .await
                .is_some()
        );

        // A stash may share the stem too: opened from the stash, the
        // page is stashed whatever the plans say, and closes with the
        // stash though an active foo exists.
        s.stash.push(crate::cli::status::StashItemView {
            stem: "foo".into(),
            waiting_for: None,
            ready: false,
            commits: 2,
        });
        assert_eq!(
            plan_page_facts(p, "foo", &s, PlanSource::Stash).await,
            Some((input::PlanPageState::Stashed, None))
        );
        s.stash.clear();
        assert_eq!(
            plan_page_facts(p, "foo", &s, PlanSource::Stash).await,
            None,
            "the stashed page closes though an active foo exists"
        );
    }

    /// A stash row's page reads the stash, offers what a stashed plan
    /// can do, and restoring it leaves the page on the now-active plan.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stash_page_reads_the_stash_and_restores_into_an_active_plan() {
        let repo = tempfile::TempDir::new().unwrap();
        let p = repo.path();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(p)
                    .args(args)
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?}"
            )
        };
        git(&["init", "--quiet", "--initial-branch=main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgsign", "false"]);
        let write = |rel: &str, body: &str| {
            std::fs::create_dir_all(p.join(rel).parent().unwrap()).unwrap();
            std::fs::write(p.join(rel), body).unwrap();
        };
        write(
            ".clank/config.json",
            r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
        );
        write(
            ".clank/.gitignore",
            "/cache/\n/agents/\n/stash/\n/shelved/\n",
        );
        write("base.rs", "// base\n");
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "base"]);
        write(".clank/plans/foo.md", "# foo\n\nthe stashed plan\n");
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "[foo] intro"]);
        run_plan_confirm(ConfirmAction::StashPlan, p, "foo")
            .await
            .unwrap();
        assert!(!p.join(".clank/plans/foo.md").exists(), "stashed");

        let page = stash_page(p, "foo");
        assert_eq!(page.st, input::PlanPageState::Stashed);
        assert!(
            page.body
                .as_deref()
                .is_some_and(|b| b.contains("the stashed plan")),
            "{:?}",
            page.body
        );
        assert_eq!(
            input::plan_actions(page.st),
            [
                input::PlanAction::OpenHtml,
                input::PlanAction::Restore,
                input::PlanAction::DropStash,
                input::PlanAction::Back
            ]
        );

        let mut open = Some(page);
        let outcome = run_plan_confirm(ConfirmAction::RestorePlan, p, "foo").await;
        settle_plan_confirm(p, ConfirmAction::RestorePlan, &outcome, &mut open);
        assert!(outcome.is_ok(), "{outcome:?}");
        assert!(p.join(".clank/plans/foo.md").exists(), "restored");
        assert_eq!(
            open.map(|pg| pg.st),
            Some(input::PlanPageState::Active { repo_paused: false })
        );
    }

    /// Dropping a stash from the page destroys it, and the page has
    /// nothing left to read.
    ///
    /// Through `run_plan_confirm` — the SAME core `clank stash drop`
    /// runs — because the TUI is a front-end and a reimplemented write
    /// here would be a second way to delete something with no undo.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_a_stash_from_its_page_destroys_it() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .current_dir(p)
                    .args(args)
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?}"
            )
        };
        git(&["init", "--quiet", "--initial-branch=main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgsign", "false"]);
        let write = |rel: &str, body: &str| {
            std::fs::create_dir_all(p.join(rel).parent().unwrap()).unwrap();
            std::fs::write(p.join(rel), body).unwrap();
        };
        write(
            ".clank/config.json",
            r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
        );
        write(
            ".clank/.gitignore",
            "/cache/\n/agents/\n/stash/\n/shelved/\n",
        );
        write("base.rs", "// base\n");
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "base"]);
        write(".clank/plans/foo.md", "# foo\n\nthe stashed plan\n");
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "[foo] intro"]);
        run_plan_confirm(ConfirmAction::StashPlan, p, "foo")
            .await
            .unwrap();

        // What the screen would capture, read the way the loop reads
        // it: from the record, not from a snapshot.
        let shown = |p: &std::path::Path| {
            crate::cli::stash::scan_stash(p)
                .into_iter()
                .find(|(stem, _)| stem == "foo")
                .and_then(|(_, rec)| {
                    crate::cli::stash::StashIdentity::of(p, &rec).map(|id| (rec.shas.len(), id))
                })
                .expect("a stash record to size")
        };
        let (commits, fingerprint) = shown(p);
        assert_eq!(commits, 1);

        let outcome = run_plan_confirm(
            ConfirmAction::DropStashedPlan {
                commits,
                identity: fingerprint,
            },
            p,
            "foo",
        )
        .await;
        assert!(outcome.is_ok(), "{outcome:?}");

        assert!(
            crate::cli::stash::scan_stash(p).is_empty(),
            "the stash is gone"
        );
        assert!(
            !p.join(".clank/plans/foo.md").exists(),
            "and it was NOT restored on the way out"
        );

        // And the page's source is gone with it, so a refresh closes
        // the page rather than finding something else to show. The
        // snapshot still lists an ACTIVE `foo` here — the stem the
        // stash shared — so this also pins that the close does not
        // fall through to it.
        use clank_core::plan_view::WaitingOn;
        let mut s = snap(Some(plan_state("foo", WaitingOn::MasterToContinue)), vec![]);
        s.stash.clear();
        assert_eq!(
            plan_page_facts(p, "foo", &s, PlanSource::Stash).await,
            None,
            "nothing left to read, and the active `foo` is not it"
        );
    }

    /// A stash REPLACED while the confirm is open is not the stash the
    /// operator agreed to destroy.
    ///
    /// The stem is a name and names are reused: pop `foo` and stash a
    /// different `foo` while the screen sits there, and answering Yes
    /// would delete whatever now answers to it. The count does not save
    /// you — the replacement here has the same one — which is why the
    /// confirm carries an identity and the delete refuses a mismatch
    /// rather than taking it (codex on c1f9301).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stash_replaced_under_the_confirm_is_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .current_dir(p)
                    .args(args)
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?}"
            )
        };
        git(&["init", "--quiet", "--initial-branch=main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgsign", "false"]);
        let write = |rel: &str, body: &str| {
            std::fs::create_dir_all(p.join(rel).parent().unwrap()).unwrap();
            std::fs::write(p.join(rel), body).unwrap();
        };
        write(
            ".clank/config.json",
            r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
        );
        write(
            ".clank/.gitignore",
            "/cache/\n/agents/\n/stash/\n/shelved/\n",
        );
        write("base.rs", "// base\n");
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "base"]);
        let base = String::from_utf8(
            std::process::Command::new("git")
                .current_dir(p)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        write(".clank/plans/foo.md", "# foo\n\nthe FIRST foo\n");
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "[foo] intro"]);
        run_plan_confirm(ConfirmAction::StashPlan, p, "foo")
            .await
            .unwrap();

        let captured = crate::cli::stash::scan_stash(p)
            .into_iter()
            .find(|(stem, _)| stem == "foo")
            .and_then(|(_, rec)| {
                crate::cli::stash::StashIdentity::of(p, &rec).map(|id| (rec.shas.len(), id))
            })
            .expect("the stash the screen would show");

        // …and now somebody else discards it and stashes a DIFFERENT
        // `foo` under the same name, while the confirm is still up.
        // Built to the SAME SHAPE — one commit — so the count cannot be
        // what tells them apart.
        run_plan_confirm(ConfirmAction::RestorePlan, p, "foo")
            .await
            .unwrap();
        git(&["reset", "--hard", "--quiet", &base]);
        write(".clank/plans/foo.md", "# foo\n\nthe SECOND foo\n");
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "[foo] intro"]);
        run_plan_confirm(ConfirmAction::StashPlan, p, "foo")
            .await
            .unwrap();
        let now = crate::cli::stash::scan_stash(p)
            .into_iter()
            .find(|(stem, _)| stem == "foo")
            .map(|(_, rec)| rec.shas.len())
            .expect("a replacement stash");
        assert_eq!(
            now, captured.0,
            "the control: the count MATCHES, so a count-only confirm would have gone ahead"
        );

        let outcome = run_plan_confirm(
            ConfirmAction::DropStashedPlan {
                commits: captured.0,
                identity: captured.1,
            },
            p,
            "foo",
        )
        .await;

        let err = outcome.expect_err("a replaced stash must not be dropped");
        assert!(
            format!("{err:#}").contains("not the stash you were shown"),
            "and it says why: {err:#}"
        );
        assert!(
            !crate::cli::stash::scan_stash(p).is_empty(),
            "the replacement is still there — nothing was destroyed"
        );
    }

    /// Two queue files parsing to one name — here to one name AND one
    /// priority, which the lenient parse allows (`foo.md`, `999-foo.md`)
    /// — are two entries. Each row opens its OWN body, each page
    /// refreshes onto its own file, an entry gone with a duplicate
    /// left closes its page rather than retarget, and a lone
    /// survivor's rename is followed (codex on 2b35cb8, 791a448).
    #[tokio::test]
    async fn duplicate_queue_names_open_and_refresh_as_distinct_entries() {
        use crate::cli::status_tui::fixtures::snap;
        let repo = tempfile::TempDir::new().unwrap();
        let p = repo.path();
        std::fs::create_dir_all(p.join(".clank/queue")).unwrap();
        let bare = p.join(".clank/queue/foo.md");
        let prefixed = p.join(".clank/queue/999-foo.md");
        std::fs::write(&bare, "# bare foo").unwrap();
        std::fs::write(&prefixed, "# prefixed foo").unwrap();
        let entries = crate::cli::queue::scan_queue(p);
        assert_eq!(entries.len(), 2, "both files are entries");
        assert_eq!(
            (entries[0].priority, entries[1].priority),
            (999, 999),
            "and the parse cannot tell them apart"
        );
        let mut s = snap(None, vec![]);
        s.queue = entries
            .iter()
            .map(|e| crate::cli::status::QueueItemView {
                priority: e.priority,
                name: e.name.clone(),
                path: e.path.clone(),
            })
            .collect();

        for (path, body) in [(&bare, "# bare foo"), (&prefixed, "# prefixed foo")] {
            let mut page = Some(PlanPage {
                stem: "foo".into(),
                st: input::PlanPageState::Queued { priority: 999 },
                entry: Some(path.clone()),
                body: None,
                scroll: 0,
                drafts_row: None,
            });
            assert!(refetch_plan_page(p, &mut page, &s).await);
            let pg = page.unwrap();
            assert_eq!(pg.entry.as_deref(), Some(path.as_path()));
            assert_eq!(pg.body.as_deref(), Some(body), "{}", path.display());
        }
        // The prefixed entry renamed from a shell while its page is
        // open: its file is gone and `foo` is still ambiguous — the
        // page closes rather than show the other file.
        s.queue.retain(|q| q.path != prefixed);
        let renamed = p.join(".clank/queue/300-foo.md");
        std::fs::rename(&prefixed, &renamed).unwrap();
        s.queue.push(crate::cli::status::QueueItemView {
            priority: 300,
            name: "foo".into(),
            path: renamed.clone(),
        });
        let mut page = Some(PlanPage {
            stem: "foo".into(),
            st: input::PlanPageState::Queued { priority: 999 },
            entry: Some(prefixed.clone()),
            body: None,
            scroll: 0,
            drafts_row: None,
        });
        assert!(!refetch_plan_page(p, &mut page, &s).await);
        assert!(page.is_none());
        // With one `foo` left, the rename is followed: new file, new
        // priority, its body.
        s.queue.retain(|q| q.path != bare);
        let mut page = Some(PlanPage {
            stem: "foo".into(),
            st: input::PlanPageState::Queued { priority: 999 },
            entry: Some(prefixed.clone()),
            body: None,
            scroll: 0,
            drafts_row: None,
        });
        assert!(refetch_plan_page(p, &mut page, &s).await);
        let pg = page.unwrap();
        assert_eq!(pg.st, input::PlanPageState::Queued { priority: 300 });
        assert_eq!(pg.entry.as_deref(), Some(renamed.as_path()));
        assert_eq!(pg.body.as_deref(), Some("# prefixed foo"));
    }

    /// A priority change from the page renames the file, and the
    /// page's identity follows the rename, so the refresh after it
    /// finds the page's own file rather than a name it must guess at.
    #[test]
    fn a_priority_change_moves_the_page_with_its_file() {
        let repo = tempfile::TempDir::new().unwrap();
        let p = repo.path();
        std::fs::create_dir_all(p.join(".clank/queue")).unwrap();
        let before = p.join(".clank/queue/500-foo.md");
        std::fs::write(&before, "# foo").unwrap();
        let mut page = PlanPage {
            stem: "foo".into(),
            st: input::PlanPageState::Queued { priority: 500 },
            entry: Some(before.clone()),
            body: None,
            scroll: 0,
            drafts_row: None,
        };
        set_priority_to(p, &mut page, 510).unwrap();
        let after = p.join(".clank/queue/510-foo.md");
        assert!(after.exists() && !before.exists(), "the file was renamed");
        assert_eq!(page.st, input::PlanPageState::Queued { priority: 510 });
        assert_eq!(page.entry.as_deref(), Some(after.as_path()));
        // The page clamps before it writes, so the ceiling arrives
        // here already applied.
        set_priority_to(p, &mut page, 999).unwrap();
        assert_eq!(page.st, input::PlanPageState::Queued { priority: 999 });
        assert!(p.join(".clank/queue/999-foo.md").exists());
        // An ambiguous name is refused, and the page is left as it was.
        std::fs::write(p.join(".clank/queue/foo.md"), "# other foo").unwrap();
        assert!(set_priority_to(p, &mut page, 989).is_err());
        assert_eq!(page.st, input::PlanPageState::Queued { priority: 999 });
        // A page that is not queued has no priority to change.
        let mut active = PlanPage {
            stem: "foo".into(),
            st: input::PlanPageState::Active { repo_paused: false },
            entry: None,
            body: None,
            scroll: 0,
            drafts_row: None,
        };
        set_priority_to(p, &mut active, 510).unwrap();
        assert_eq!(
            active.st,
            input::PlanPageState::Active { repo_paused: false }
        );
    }

    /// `o` from a queued page opens ITS file's page — the stem, so the
    /// two equal-name rows reach two pages — and an active page opens
    /// the plan's.
    #[test]
    fn a_queued_page_opens_its_own_files_html() {
        let page = |entry: Option<&str>, st| PlanPage {
            stem: "foo".into(),
            st,
            entry: entry.map(std::path::PathBuf::from),
            body: None,
            scroll: 0,
            drafts_row: None,
        };
        let queued = input::PlanPageState::Queued { priority: 999 };
        assert_eq!(
            queue_html_handle(&page(Some("/q/.clank/queue/999-foo.md"), queued)).as_deref(),
            Some("999-foo")
        );
        assert_eq!(
            queue_html_handle(&page(Some("/q/.clank/queue/foo.md"), queued)).as_deref(),
            Some("foo")
        );
        assert_eq!(
            queue_html_handle(&page(None, queued)),
            None,
            "no file, no page to open — never a name that could be another's"
        );
        assert_eq!(
            queue_html_handle(&page(
                None,
                input::PlanPageState::Active { repo_paused: false }
            )),
            None
        );
    }

    /// The refresh keeps the page on its source and re-reads that
    /// source's body: the same-stem regression, both directions.
    #[tokio::test]
    async fn a_refresh_never_flips_a_page_between_the_queue_and_the_plans() {
        use crate::cli::status_tui::fixtures::{plan_state, snap};
        use clank_core::plan_view::WaitingOn;
        let repo = tempfile::TempDir::new().unwrap();
        let p = repo.path();
        std::fs::create_dir_all(p.join(".clank/plans")).unwrap();
        std::fs::create_dir_all(p.join(".clank/queue")).unwrap();
        std::fs::write(p.join(".clank/plans/foo.md"), "# foo active").unwrap();
        let entry = p.join(".clank/queue/300-foo.md");
        std::fs::write(&entry, "# foo queued").unwrap();
        let mut s = snap(
            Some(plan_state("foo", WaitingOn::MasterToContinue)),
            vec!["foo"],
        );
        s.queue[0].priority = 300;
        s.queue[0].path = entry.clone();

        let mut queued = Some(PlanPage {
            stem: "foo".into(),
            st: input::PlanPageState::Queued { priority: 300 },
            entry: Some(entry),
            body: None,
            scroll: 0,
            drafts_row: None,
        });
        assert!(refetch_plan_page(p, &mut queued, &s).await);
        let q = queued.unwrap();
        assert_eq!(q.st, input::PlanPageState::Queued { priority: 300 });
        assert_eq!(q.body.as_deref(), Some("# foo queued"));

        let mut active = Some(PlanPage {
            stem: "foo".into(),
            st: input::PlanPageState::Active { repo_paused: false },
            entry: None,
            body: None,
            scroll: 0,
            drafts_row: None,
        });
        assert!(refetch_plan_page(p, &mut active, &s).await);
        let a = active.unwrap();
        assert_eq!(a.st, input::PlanPageState::Active { repo_paused: false });
        assert_eq!(a.body.as_deref(), Some("# foo active"));
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
        )
        .unwrap();
        let cfg = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        assert!(
            !cfg.contains("codex"),
            "reviewer removed from the local roster via the core: {cfg}"
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
            ConfirmAction::AddCandidate {
                idx: 0,
                tier: crate::cli::teams_config::ReviewKind::Gate,
            },
            repo.path(),
            Some(home.path()),
            &s,
            &picker,
        )
        .unwrap();
        let cfg = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        assert!(
            cfg.contains("ruthless"),
            "candidate added to the local roster via the core: {cfg}"
        );
        // The CHOSEN tier, not the default: adding `ruthless` at `gate`
        // and getting `commit` is the exact defect this carries a tier
        // to prevent, and it is invisible unless the role is asserted.
        let parsed: serde_json::Value = serde_json::from_str(&cfg).unwrap();
        assert_eq!(
            parsed["agents"]["ruthless"]["role"], "gate",
            "the tier chosen at add is the tier written: {cfg}"
        );
    }

    /// Every reviewer tier reaches the roster through the confirm path.
    #[test]
    fn apply_confirm_add_writes_each_reviewer_tier() {
        use crate::cli::teams_config::ReviewKind;
        for (tier, want) in [
            (ReviewKind::Commit, "commit"),
            (ReviewKind::Plan, "plan"),
            (ReviewKind::Final, "final"),
            (ReviewKind::Gate, "gate"),
        ] {
            let repo = tempfile::TempDir::new().unwrap();
            let home = tempfile::TempDir::new().unwrap();
            std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
            std::fs::write(
                repo.path().join(".clank/config.json"),
                r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
            )
            .unwrap();
            std::fs::create_dir_all(home.path().join(".clank")).unwrap();
            std::fs::write(
                home.path().join(".clank/config.json"),
                r#"{"agents":{"ruthless":{"tool":"claude"}},"teams":{}}"#,
            )
            .unwrap();
            let s = two_agent_snap();
            let picker = vec![cand("ruthless", "claude", "claude")];
            apply_confirm(
                ConfirmAction::AddCandidate { idx: 0, tier },
                repo.path(),
                Some(home.path()),
                &s,
                &picker,
            )
            .unwrap();
            let cfg = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&cfg).unwrap();
            assert_eq!(
                parsed["agents"]["ruthless"]["role"], want,
                "{tier:?} must write `{want}`: {cfg}"
            );
        }
    }

    /// `master` is unrepresentable in the add flow, not merely rejected.
    ///
    /// `AddCandidate` carries a `ReviewKind`, which has no `Master`
    /// variant, so the invalid state cannot be built. This pins the
    /// property the type provides: adding a master would mean demoting
    /// the incumbent, which is `clank agent make-master`'s job.
    #[test]
    fn no_reviewer_tier_can_become_master() {
        use crate::cli::teams_config::{ReviewKind, RosterRole};
        for tier in [
            ReviewKind::Commit,
            ReviewKind::Plan,
            ReviewKind::Final,
            ReviewKind::Gate,
        ] {
            assert_ne!(
                RosterRole::from(tier),
                RosterRole::Master,
                "{tier:?} must never map to master"
            );
        }
    }

    #[test]
    fn apply_detail_action_tier_checkbox_uses_the_core_and_stays_on_page() {
        use crate::cli::teams_config::RosterRole;
        let repo = detail_repo();
        let mut s = two_agent_snap(); // idx 1 == codex (commit)
        // Tick `plan` on a commit-tier reviewer → leaves commit mode.
        let next = apply_detail_action(
            DetailAction::TierPlan,
            1,
            2,
            &mut s,
            repo.path(),
            &mut None,
            1,
        );
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
            1,
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

        // Unticking down to the last box and then that one leaves the
        // agent reviewing nothing: standby, written through the core.
        apply_detail_action(
            DetailAction::TierPlan,
            1,
            2,
            &mut s,
            repo.path(),
            &mut None,
            1,
        );
        apply_detail_action(
            DetailAction::TierFinal,
            1,
            3,
            &mut s,
            repo.path(),
            &mut None,
            1,
        );
        let parsed: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(parsed["agents"]["codex"]["role"], "standby");
        assert_eq!(s.agents[1].role, RosterRole::Standby);
    }

    fn bind(repo: &std::path::Path, label: &str, id: &str) {
        let cfg = clank_core::AgentConfig {
            session: Some(bound(clank_core::vocab::Tool::Codex, id)),
            ..Default::default()
        };
        crate::agent_store::update_agent_config(
            repo,
            &clank_core::ids::AgentLabel::parse(label).unwrap(),
            |stored| *stored = cfg,
        )
        .unwrap();
    }

    fn bound_id(repo: &std::path::Path, label: &str) -> Option<String> {
        crate::agent_store::load_agent_config(
            repo,
            &clank_core::ids::AgentLabel::parse(label).unwrap(),
        )
        .unwrap()
        .and_then(|c| c.session)
        .map(|s| s.id.as_str().to_string())
    }

    fn target(label: &str, id: &str) -> crate::cli::status_tui::input::ClearTarget {
        crate::cli::status_tui::input::ClearTarget {
            label: label.to_string(),
            session: bound(clank_core::vocab::Tool::Codex, id),
        }
    }

    #[test]
    fn a_binding_that_changed_after_the_confirm_showed_it_is_not_cleared() {
        let repo = detail_repo();
        bind(repo.path(), "codex", "session-b");
        let mut s = two_agent_snap();
        s.agents[1].session = Some(bound(clank_core::vocab::Tool::Codex, "session-a"));
        let mut open = None;

        let next = clear_target_session(
            repo.path(),
            &mut s,
            Some(&target("codex", "session-a")),
            &mut open,
        );

        assert_eq!(next, Mode::AgentDetail { idx: 1, sel: 0 });
        assert_eq!(bound_id(repo.path(), "codex").as_deref(), Some("session-b"));
        let said = open.expect("the page says nothing was cleared");
        let OverlayData::Error { title, message } = &said.data else {
            panic!("a refusal is an error overlay");
        };
        assert_eq!(title, "nothing was cleared");
        assert!(message.contains("changed"), "{message}");
    }

    /// Keys arrive in batches: Enter on a panel row and `u` can both be
    /// handled before any frame of the page is painted or settled. The
    /// keystroke that opens the confirm must capture what it releases.
    #[test]
    fn a_batched_enter_then_u_opens_a_confirm_that_knows_its_session() {
        use crate::cli::status_tui::input::{
            ClearTarget, DetailNav, Presence, agent_detail_nav, detail_actions, open_clear_confirm,
        };
        let repo = detail_repo();
        bind(repo.path(), "codex", "session-a");
        let mut s = two_agent_snap();
        s.agents[1].session = Some(bound(clank_core::vocab::Tool::Codex, "session-a"));

        // Enter on codex's panel row opened its page; nothing settled.
        let actions = detail_actions(s.agents[1].role, Presence::Unknown, true);
        let nav = agent_detail_nav(0, &actions, Key::Char(b'u'));
        assert_eq!(nav, DetailNav::Activate(DetailAction::ClearSession));
        let (mode, target) = open_clear_confirm(&s.agents[1]).expect("codex is bound");

        assert_eq!(
            mode,
            Mode::Confirm {
                action: ConfirmAction::ClearSession { scroll: 0 }
            }
        );
        assert_eq!(
            target,
            ClearTarget {
                label: "codex".to_string(),
                session: bound(clank_core::vocab::Tool::Codex, "session-a"),
            }
        );
        let mut open = None;
        clear_target_session(repo.path(), &mut s, Some(&target), &mut open);
        assert!(open.is_none(), "the batched confirm clears what it named");
        assert_eq!(bound_id(repo.path(), "codex"), None);
    }

    /// A batch that leaves one agent's painted page for another's and
    /// presses `u` targets the agent it landed on, not the session the
    /// last frame showed.
    #[test]
    fn a_batch_that_switches_agents_clears_the_agent_it_landed_on() {
        use crate::cli::status_tui::input::{SeenSession, open_clear_confirm, settle_seen_session};
        let repo = detail_repo();
        bind(repo.path(), "claude", "session-a");
        bind(repo.path(), "codex", "session-b");
        let mut s = two_agent_snap();
        s.agents[0].session = Some(bound(clank_core::vocab::Tool::Codex, "session-a"));
        s.agents[1].session = Some(bound(clank_core::vocab::Tool::Codex, "session-b"));
        let mut seen: Option<SeenSession> = None;
        settle_seen_session(
            Mode::AgentDetail { idx: 0, sel: 0 },
            &s.agents,
            &mut seen,
            |_, _| Some(true),
        );
        assert_eq!(seen.as_ref().map(|s| s.id.as_str()), Some("session-a"));

        let (_, target) = open_clear_confirm(&s.agents[1]).expect("codex is bound");
        assert_eq!(
            (target.label.as_str(), target.session.id.as_str()),
            ("codex", "session-b")
        );
        let mut open = None;
        clear_target_session(repo.path(), &mut s, Some(&target), &mut open);
        assert_eq!(bound_id(repo.path(), "codex"), None);
        assert_eq!(
            bound_id(repo.path(), "claude").as_deref(),
            Some("session-a"),
            "the page painted last is untouched"
        );
    }

    #[test]
    fn an_unbound_agent_opens_no_clear_confirm() {
        let s = two_agent_snap();
        assert_eq!(
            crate::cli::status_tui::input::open_clear_confirm(&s.agents[1]),
            None
        );
    }

    #[test]
    fn apply_detail_action_failed_promote_surfaces_the_error_and_stays() {
        // codex e4bccc5: set_repo_master failures were `let _ =`
        // swallowed — a silently unchanged roster reads as "clank
        // ignored me". A failing core (no repo at the path) must
        // surface through the error out-param and keep the page open.
        let mut s = two_agent_snap();
        let mut err: Option<Overlay> = None;
        let next = apply_detail_action(
            DetailAction::PromoteToMaster,
            1,
            4,
            &mut s,
            std::path::Path::new("/nonexistent/clank-test-repo"),
            &mut err,
            1,
        );
        let opened = err.expect("failure must be reported, not swallowed");
        let OverlayData::Error { title, message } = &opened.data else {
            panic!("a failure opens the FAILURE overlay, not another kind");
        };
        assert_eq!(title, "promote failed");
        assert!(!message.is_empty());
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
            1,
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

    /// Swap acts on a SECOND operand, so the action itself must not
    /// write — it opens the picker and nothing else changes.
    #[test]
    fn apply_detail_action_swap_only_opens_the_picker() {
        let repo = detail_repo();
        let mut s = two_agent_snap();
        let before = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        let mode = apply_detail_action(DetailAction::Swap, 1, 0, &mut s, repo.path(), &mut None, 1);
        assert_eq!(
            mode,
            Mode::SwapPicker { out: 1, sel: 0 },
            "opens on that agent"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap(),
            before,
            "no write until a candidate is chosen"
        );
    }

    /// The whole action is ONE config write, and the roster change is
    /// its only observable effect.
    #[test]
    fn apply_swap_writes_the_roster_and_carries_the_role() {
        let repo = detail_repo();
        let mut s = two_agent_snap();
        let picker = vec![crate::cli::status::AvailableAgent {
            label: "scout".to_string(),
            tool: "codex".to_string(),
            invocation: "codex".to_string(),
            description: None,
        }];
        let home = library_home(&["scout"]);
        let mut err = None;
        let mode = apply_swap(
            1,
            0,
            &mut s,
            &picker,
            repo.path(),
            Some(home.path()),
            &mut err,
        );

        assert_eq!(mode, Mode::AgentPanel { sel: 0 }, "back to the panel");
        assert!(err.is_none(), "clean swap reports nothing: {err:?}");

        let parsed: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap(),
        )
        .unwrap();
        assert!(
            parsed["agents"]["scout"].is_object(),
            "incoming is on the roster"
        );
        assert!(parsed["agents"]["codex"].is_null(), "outgoing is off it");
        assert_eq!(
            parsed["agents"]["scout"]["role"], "commit",
            "and it carried the role across"
        );

        // Sole pane ownership is asserted at the PaneIo seam instead —
        // see `a_swap_reaches_the_panes_only_through_the_reconciler`.
        // A filesystem check cannot show it: a direct zellij call is
        // IPC and leaves no artifact behind.
    }

    #[test]
    fn apply_swap_replaces_the_master_from_the_same_picker_path() {
        let repo = detail_repo();
        let mut s = two_agent_snap();
        let picker = vec![crate::cli::status::AvailableAgent {
            label: "scout".to_string(),
            tool: "codex".to_string(),
            invocation: "codex".to_string(),
            description: None,
        }];
        let home = library_home(&["scout"]);
        let mut err = None;

        let mode = apply_swap(
            0,
            0,
            &mut s,
            &picker,
            repo.path(),
            Some(home.path()),
            &mut err,
        );

        assert_eq!(mode, Mode::AgentPanel { sel: 0 });
        assert!(err.is_none(), "master swap reports nothing: {err:?}");
        let cfg = crate::agent_store::load_repo_config_required(repo.path()).unwrap();
        let scout = clank_core::ids::AgentLabel::parse("scout").unwrap();
        let old = clank_core::ids::AgentLabel::parse("claude").unwrap();
        assert_eq!(
            cfg.agents[&scout].role,
            crate::cli::teams_config::RosterRole::Master
        );
        assert_eq!(cfg.agents[&scout].tool, clank_core::vocab::Tool::Codex);
        assert!(!cfg.agents.contains_key(&old));
    }

    // ── the wait page's two identities ────────────────────────
    //
    // Every case below must resolve to NO target, and each drives the
    // REAL path: `resolve_kill_target` reads the record from disk, so
    // these write and rewrite that file rather than handing the
    // resolver a synthetic snapshot it could never have seen.

    fn attended(
        task: &str,
        at: &str,
        pid: Option<i32>,
        token: bool,
    ) -> crate::cli::agent_state::Attended {
        crate::cli::agent_state::Attended {
            expect: None,
            task: task.to_string(),
            desc: Some("test run".to_string()),
            pid,
            token: token
                .then(|| pid.and_then(crate::proc_identity::token_for))
                .flatten(),
            at: at.to_string(),
            subagent: false,
            pending: false,
            check_in: None,
        }
    }

    fn page(label: &str, task: &str, at: &str) -> crate::cli::status_tui::input::WaitPage {
        WaitPage::new(
            label,
            &attended(task, at, Some(std::process::id() as i32), true),
        )
    }

    /// Publish an attendance where the resolver will look for it: the
    /// agent's state as its monitor writes it.
    fn write_attended(
        repo: &std::path::Path,
        label: &str,
        att: &crate::cli::agent_state::Attended,
    ) {
        let dir = repo.join(".clank/agents").join(label);
        std::fs::create_dir_all(&dir).unwrap();
        crate::cli::agent_state::store(
            &dir,
            &crate::cli::agent_state::AgentState {
                role: "master".into(),
                session: "s".into(),
                tool: clank_core::Tool::Claude,
                monitor: crate::cli::agent_state::Observer::me(),
                activity: crate::cli::agent_state::Activity::Idle,
                since: 0,
                suppressors: Vec::new(),
                nudge: crate::cli::agent_state::Nudge::Nothing,
                resumes: None,
                cannot_resume: None,
                attending: Some(att.clone()),
                asking: None,
                repeated: None,
            },
        );
    }

    fn snap_attending(
        att: Option<crate::cli::agent_state::Attended>,
    ) -> crate::cli::status::StatusSnapshot {
        let mut s = two_agent_snap();
        s.agents[0].attending = att;
        s
    }

    /// Activating the visible Back ROW — not Escape, which takes a
    /// different path — must return to the agent's ROW, not to its
    /// index. With a wait above, those differ, and Back highlighted
    /// whatever now sits at the index.
    #[test]
    fn back_from_the_detail_page_lands_on_the_agent_row_not_its_index() {
        let repo = detail_repo();
        let me = std::process::id() as i32;
        let mut s = snap_attending(Some(attended("t1", "2026-08-20T14:51:09Z", Some(me), true)));
        s.stash = vec![];
        s.queue = vec![];
        // [Agent(0), Wait(0), Agent(1), Add] — agent 1 is at ROW 2.
        let rows = panel_row_list(&s, 120);
        let agent_row = row_position(&rows, crate::cli::status_tui::input::PanelRow::Agent(1));
        assert_eq!(agent_row, 2, "precondition: the wait shifted it");

        let next = apply_detail_action(
            DetailAction::Back,
            1,
            0,
            &mut s,
            repo.path(),
            &mut None,
            agent_row,
        );
        assert_eq!(
            next,
            Mode::AgentPanel { sel: 2 },
            "Back returns to the agent's row"
        );
        assert_ne!(next, Mode::AgentPanel { sel: 1 }, "not to its index");
    }

    /// A refresh cancels an open confirm back to the panel. A KILL
    /// confirm necessarily has a wait row on screen, so the add-row
    /// position is never `agents.len()` there — it landed on another
    /// agent or wait every time.
    #[test]
    fn a_refresh_cancelling_a_kill_confirm_lands_on_the_add_row() {
        use crate::cli::status_tui::input::add_row_index;
        let me = std::process::id() as i32;
        let mut s = snap_attending(Some(attended("t1", "2026-08-20T14:51:09Z", Some(me), true)));
        s.stash = vec![];
        s.queue = vec![];
        let rows = panel_row_list(&s, 120);
        assert_eq!(
            add_row_index(&rows),
            3,
            "A0, W0, A1, Add — and `agents.len()` is 2"
        );
        assert_ne!(
            add_row_index(&rows),
            s.agents.len(),
            "the old fallback would have selected the wait's agent"
        );
    }

    /// Every transition that names a panel row goes through
    /// `row_position`, and a wait above the target is what breaks the
    /// arithmetic those sites used to do for themselves: agent-detail
    /// Back returned the AGENT INDEX, queue reprioritise computed
    /// `agents.len() + 1 + pos`, and the refresh fallbacks passed
    /// `agents.len()` as the add row (codex on bc033f0).
    #[test]
    fn a_wait_above_the_target_moves_every_named_row() {
        use crate::cli::status_tui::input::{PanelRow, add_row_index};
        let me = std::process::id() as i32;
        let mut s = snap_attending(Some(attended("t1", "2026-08-20T14:51:09Z", Some(me), true)));
        s.stash = vec![];
        s.queue = vec![];

        // Wide: agent 0's wait is drawable, so it takes a position.
        let with = panel_row_list(&s, 120);
        assert_eq!(with.len(), 5, "A0, W0, A1, Add, R: {with:?}");
        assert_eq!(
            row_position(&with, PanelRow::Agent(1)),
            2,
            "the agent-detail Back target is NOT the agent index"
        );
        assert_eq!(add_row_index(&with), 3, "nor is the add row agents.len()");

        // Same snapshot, a pane too narrow to draw the wait: the row
        // is gone and every position closes back up.
        let narrow = panel_row_list(&s, 6);
        assert_eq!(narrow.len(), 4, "A0, A1, Add, R: {narrow:?}");
        assert_eq!(row_position(&narrow, PanelRow::Agent(1)), 1);
        assert_eq!(add_row_index(&narrow), 2);

        // A row that no longer exists falls back to "+ add" rather
        // than to some other agent's row.
        assert_eq!(
            row_position(&narrow, PanelRow::Wait(0)),
            add_row_index(&narrow)
        );
    }

    /// Up at the log's top lands on the row drawn directly above the
    /// log, past a wait row, the queue, the drafts and grouped sources:
    /// the last source row. Down from it returns to the log's first entry.
    #[test]
    fn up_from_the_log_lands_on_the_row_above_it() {
        use crate::cli::status_tui::input::{PanelAction, PanelRow, agent_panel_action};
        let me = std::process::id() as i32;
        let mut s = snap_attending(Some(attended("t1", "2026-08-20T14:51:09Z", Some(me), true)));
        s.queue = vec![crate::cli::status::QueueItemView {
            priority: 500,
            name: "queued-idea".into(),
            path: std::path::PathBuf::new(),
        }];
        s.drafts = vec![crate::cli::queue::Draft {
            name: "idea".into(),
            path: "idea.md".into(),
            title: None,
            modified: None,
        }];
        s.sources = vec![
            command_source("alpha", "signal"),
            command_source("alpha", "voice"),
            command_source("beta", "voice"),
        ];
        let rows = panel_row_list(&s, 120);
        assert!(
            rows.contains(&PanelRow::Wait(0)),
            "premise: a wait row {rows:?}"
        );
        let topped = LogView::new();
        let Some(Mode::AgentPanel { sel }) = log_up_crossing(&topped, &s, 120) else {
            panic!("crosses");
        };
        assert_eq!(rows[sel], PanelRow::Source(1), "the voice row, the last");
        assert_eq!(
            agent_panel_action(sel, &rows, crate::cli::status_tui::input::Key::Down),
            PanelAction::EnterLog
        );

        let mut bare = two_agent_snap();
        bare.stash = vec![];
        bare.queue = vec![];
        let rows = panel_row_list(&bare, 120);
        let Some(Mode::AgentPanel { sel }) = log_up_crossing(&topped, &bare, 120) else {
            panic!("crosses");
        };
        assert_eq!(rows[sel], PanelRow::Remote, "the last row, not + add");

        let mut mid = LogView::new();
        mid.cursor = 3;
        mid.offset = 2;
        assert_eq!(log_up_crossing(&mid, &s, 120), None, "mid-log");
        let mut burst = LogView::new();
        burst.offset = 4;
        assert_eq!(
            log_up_crossing(&burst, &s, 120),
            None,
            "viewport mid-scroll"
        );
        s.agents.clear();
        assert_eq!(log_up_crossing(&topped, &s, 120), None, "no roster");
    }

    /// An agent whose label no longer parses leaves its page for its own
    /// row, not for whatever row sits at its index.
    #[test]
    fn a_detail_page_on_an_unparseable_agent_returns_to_its_row() {
        let repo = detail_repo();
        let me = std::process::id() as i32;
        let mut s = snap_attending(Some(attended("t1", "2026-08-20T14:51:09Z", Some(me), true)));
        s.agents[1].label = "a/b".into();
        let rows = panel_row_list(&s, 120);
        let agent_row = row_position(&rows, crate::cli::status_tui::input::PanelRow::Agent(1));
        assert_ne!(agent_row, 1, "premise: the wait above moves it");
        let next = apply_detail_action(
            DetailAction::ToggleAuto,
            1,
            0,
            &mut s,
            repo.path(),
            &mut None,
            agent_row,
        );
        assert_eq!(next, Mode::AgentPanel { sel: agent_row });
    }

    /// The queue-row target, with a wait above it: the old formula
    /// (`agents.len() + 1 + pos`) lands one row short.
    #[test]
    fn a_queue_row_target_accounts_for_waits_above_it() {
        use crate::cli::status_tui::input::PanelRow;
        let me = std::process::id() as i32;
        let mut s = snap_attending(Some(attended("t1", "2026-08-20T14:51:09Z", Some(me), true)));
        s.stash = vec![];
        let rows = panel_row_list(&s, 120);
        let queue_len = s.queue.len();
        if queue_len > 0 {
            let want = 3 + 1 + 0; // A0, W0, A1, Add, Q0
            assert_eq!(row_position(&rows, PanelRow::Queue(0)), want);
            assert_ne!(
                row_position(&rows, PanelRow::Queue(0)),
                s.agents.len() + 1,
                "the old arithmetic is off by the wait row"
            );
        }
    }

    /// The page follows its attendance across a roster REORDER — the
    /// same instance stays selected, and explicitly not the stale
    /// index it opened at.
    #[test]
    fn the_wait_page_follows_its_instance_across_a_reorder() {
        let me = std::process::id() as i32;
        let mut s = snap_attending(Some(attended("t1", "2026-08-20T14:51:09Z", Some(me), true)));
        let p = page("claude", "t1", "2026-08-20T14:51:09Z");
        assert_eq!(relocate_wait(&p, &s.agents), Some(0));

        s.agents.swap(0, 1);
        assert_eq!(
            relocate_wait(&p, &s.agents),
            Some(1),
            "found by identity at its new position"
        );

        // Same agent, attending something else now.
        s.agents[1].attending = Some(attended("t9", "2026-08-24T09:00:00Z", Some(me), true));
        assert_eq!(
            relocate_wait(&p, &s.agents),
            None,
            "a different wait is not this one"
        );

        s.agents.clear();
        assert_eq!(relocate_wait(&p, &s.agents), None);
    }

    #[test]
    fn wait_process_repaint_and_snapshot_refresh_do_not_request_another_read() {
        use crate::proc_identity::inspect::Inspection;
        let me = std::process::id() as i32;
        let mut snap = snap_attending(Some(attended("t1", "same", Some(me), true)));
        let mut page = Some(WaitPage::new(
            "claude",
            snap.agents[0].attending.as_ref().unwrap(),
        ));
        let mut mode = Mode::WaitDetail { agent: 0, sel: 0 };
        let mut process = wait_process::Controller::default();
        process.request(page.as_ref().unwrap().clone());
        let job = process.take_job().unwrap();
        let generation = job.generation;
        process.complete(wait_process::Completion {
            command: None,
            request: job,
            inspection: Inspection::Ended,
        });
        snap.agents.swap(0, 1);
        for _ in 0..100 {
            reconcile_wait_process(&mut mode, &mut page, &snap.agents, &mut process);
            assert!(process.take_job().is_none());
            assert_eq!(
                process.slot.as_ref().unwrap().request.generation,
                generation
            );
            assert_eq!(mode, Mode::WaitDetail { agent: 1, sel: 0 });
        }
    }

    #[test]
    fn wait_process_replacement_or_closed_page_discards_pending_work() {
        use crate::proc_identity::inspect::Inspection;
        let me = std::process::id() as i32;
        for replacement in 0..3 {
            let mut snap = snap_attending(Some(attended("t1", "same", Some(me), true)));
            let mut page = Some(WaitPage::new(
                "claude",
                snap.agents[0].attending.as_ref().unwrap(),
            ));
            let mut mode = Mode::WaitDetail { agent: 0, sel: 0 };
            let mut process = wait_process::Controller::default();
            process.request(page.as_ref().unwrap().clone());
            let job = process.take_job().unwrap();
            process.request(page.as_ref().unwrap().clone()); // pending explicit refresh
            match replacement {
                0 => snap.agents[0].attending.as_mut().unwrap().pid = Some(42),
                1 => snap.agents[0].attending.as_mut().unwrap().token = None,
                _ => mode = Mode::AgentPanel { sel: 0 },
            }
            // Task and timestamp did not change: PID/birth changes still invalidate.
            reconcile_wait_process(&mut mode, &mut page, &snap.agents, &mut process);
            assert!(page.is_none());
            assert!(process.slot.is_none());
            assert_eq!(mode, Mode::AgentPanel { sel: 0 });
            assert!(!process.complete(wait_process::Completion {
                command: None,
                request: job,
                inspection: Inspection::Ended
            }));
            assert!(process.take_job().is_none());
        }
    }

    #[test]
    fn wait_process_completions_are_data_even_behind_an_overlay() {
        use crate::proc_identity::inspect::Inspection;
        let page = page("claude", "task", "at");
        let mut process = wait_process::Controller::default();
        process.request(page);
        let job = process.take_job().unwrap();
        let ev = Ev::Process(wait_process::Completion {
            command: None,
            request: job,
            inspection: Inspection::Ended,
        });
        assert_eq!(behind_overlay(&ev), BehindOverlay::default());
        let mut batch = coalesce([ev, Ev::Resize]);
        assert!(batch.resize);
        assert!(!batch.refresh);
        assert_eq!(batch.process.len(), 1);
        assert!(process.complete(batch.process.pop().unwrap()));
        assert!(process.take_job().is_none());
    }

    /// Removing an agent leaves its local `attended` file behind, so
    /// the file alone is not authority to signal. An agent taken off
    /// the roster while the confirm was open must not be killable.
    #[test]
    fn an_agent_removed_from_the_roster_cannot_be_signalled() {
        let repo = detail_repo();
        let me = std::process::id() as i32;
        let p = page("claude", "t1", "2026-08-20T14:51:09Z");
        write_attended(
            repo.path(),
            "claude",
            &attended("t1", "2026-08-20T14:51:09Z", Some(me), true),
        );
        assert_eq!(
            resolve_kill_target(&p, repo.path()).map(crate::signal::Pid::get),
            Ok(me),
            "valid while on the roster"
        );

        // Off the roster, record deliberately LEFT in place.
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"codex":{"tool":"codex","role":"master"}}}"#,
        )
        .unwrap();
        assert!(
            repo.path().join(".clank/agents/claude/state.json").exists(),
            "the file is still there — that is the point"
        );
        let err = resolve_kill_target(&p, repo.path())
            .expect_err("a departed agent's leftover record must not be signalled");
        assert!(err.contains("roster"), "{err}");
    }

    /// The happy path, so every refusal below is meaningful.
    #[test]
    fn a_matching_instance_and_identity_resolve_to_the_recorded_pid() {
        let repo = detail_repo();
        let me = std::process::id() as i32;
        write_attended(
            repo.path(),
            "claude",
            &attended("t1", "2026-08-20T14:51:09Z", Some(me), true),
        );
        assert_eq!(
            resolve_kill_target(&page("claude", "t1", "2026-08-20T14:51:09Z"), repo.path())
                .map(crate::signal::Pid::get),
            Ok(me)
        );
    }

    /// The bug this shape exists to prevent: the record changes AFTER
    /// the confirm opens. The TUI's snapshot is refreshed by an
    /// asynchronous wake and may still hold the old record, so a
    /// resolver reading the snapshot would happily signal. Reading the
    /// FILE is what makes the answer current.
    #[test]
    fn a_record_replaced_between_the_confirm_and_the_signal_signals_nothing() {
        let repo = detail_repo();
        let me = std::process::id() as i32;
        let p = page("claude", "t1", "2026-08-20T14:51:09Z");
        write_attended(
            repo.path(),
            "claude",
            &attended("t1", "2026-08-20T14:51:09Z", Some(me), true),
        );
        assert_eq!(
            resolve_kill_target(&p, repo.path()).map(crate::signal::Pid::get),
            Ok(me),
            "valid when opened"
        );

        // The agent re-attends: same task, a NEW instance.
        write_attended(
            repo.path(),
            "claude",
            &attended("t1", "2026-08-24T09:00:00Z", Some(me), true),
        );
        let err = resolve_kill_target(&p, repo.path())
            .expect_err("a new instance must not inherit the standing confirm");
        assert!(err.contains("replaced"), "{err}");

        // Or stops attending altogether.
        std::fs::remove_file(repo.path().join(".clank/agents/claude/state.json")).unwrap();
        assert!(
            resolve_kill_target(&p, repo.path()).is_err(),
            "nothing to signal"
        );
    }

    /// The pid is live and the record looks fine — but the process
    /// holding that number is not the one recorded. This is the case
    /// that turns a kill into signalling a stranger.
    #[test]
    fn a_reused_pid_is_refused_even_though_it_is_alive() {
        let repo = detail_repo();
        let me = std::process::id() as i32;
        let mut att = attended("t1", "2026-08-20T14:51:09Z", Some(me), true);
        // Same live pid, a birth this process does not have.
        att.token = Some(crate::proc_identity::ProcToken::MacStart { sec: 1, usec: 1 });
        write_attended(repo.path(), "claude", &att);
        let err = resolve_kill_target(&WaitPage::new("claude", &att), repo.path())
            .expect_err("a mismatched identity must never be signalled");
        assert!(err.contains("reused") || err.contains("DIFFERENT"), "{err}");
    }

    /// Records written before identity existed stay viewable, and are
    /// never killable: their pid cannot be trusted to be the same
    /// process any more.
    #[test]
    fn a_record_without_a_token_is_viewable_but_not_killable() {
        let repo = detail_repo();
        let me = std::process::id() as i32;
        let att = attended("t1", "2026-08-20T14:51:09Z", Some(me), false);
        assert!(
            att.marker_fields(time::OffsetDateTime::now_utc()).is_some(),
            "still shown"
        );
        assert_eq!(att.killable_pid(), None, "but not actionable");
        write_attended(repo.path(), "claude", &att);
        assert!(resolve_kill_target(&WaitPage::new("claude", &att), repo.path()).is_err());
    }

    /// The other ways the target can change while the confirm sits on
    /// screen, each asserted separately: a check that only runs when
    /// the page OPENS passes all of them and still signals.
    #[test]
    fn a_target_that_changed_after_the_confirm_signals_nothing() {
        let repo = detail_repo();
        let me = std::process::id() as i32;
        let p = page("claude", "t1", "2026-08-20T14:51:09Z");

        // A different task under the same agent.
        write_attended(
            repo.path(),
            "claude",
            &attended("t2", "2026-08-20T14:51:09Z", Some(me), true),
        );
        assert!(
            resolve_kill_target(&p, repo.path()).is_err(),
            "different task"
        );

        // The process exited.
        write_attended(
            repo.path(),
            "claude",
            &attended("t1", "2026-08-20T14:51:09Z", Some(i32::MAX), true),
        );
        assert!(resolve_kill_target(&p, repo.path()).is_err(), "dead pid");

        // No record at all.
        let empty = detail_repo();
        assert!(
            resolve_kill_target(&p, empty.path()).is_err(),
            "no attendance"
        );
    }

    /// Resolving is READ-ONLY: it decides, it does not reap. The hook
    /// owns the record's lifetime.
    #[test]
    fn resolving_a_kill_leaves_the_record_alone() {
        let repo = detail_repo();
        let me = std::process::id() as i32;
        write_attended(
            repo.path(),
            "claude",
            &attended("t1", "2026-08-20T14:51:09Z", Some(me), true),
        );
        let _ = resolve_kill_target(&page("claude", "t1", "2026-08-20T14:51:09Z"), repo.path());
        assert!(
            repo.path().join(".clank/agents/claude/state.json").exists(),
            "the record survives"
        );
    }

    /// The identity rule, at the seam that had the bug: both ends of a
    /// swap are re-LOCATED by label after a refresh, never trusted as
    /// indices. An external roster or library edit reorders these
    /// lists, and a stale index would swap a pair the user never chose.
    #[test]
    fn a_swap_survives_a_reorder_by_relocating_both_ends() {
        use crate::cli::status::AvailableAgent;
        let cand = |l: &str| AvailableAgent {
            label: l.to_string(),
            tool: "codex".to_string(),
            invocation: "codex".to_string(),
            description: None,
        };

        let mut s = two_agent_snap();
        let out_label = s.agents[1].label.clone();
        let picker = vec![cand("ruthless"), cand("scout")];
        let in_label = picker[1].label.clone();

        s.agents.swap(0, 1);
        let picker: Vec<AvailableAgent> = vec![cand("scout"), cand("ruthless")];

        let out = relocate_detail(&out_label, &s.agents).expect("outgoing still on the roster");
        let sel = rebind_picker_sel_by_label(1, Some(&in_label), &picker)
            .expect("incoming still a candidate");
        assert_eq!(s.agents[out].label, out_label, "still the chosen agent");
        assert_eq!(picker[sel].label, in_label, "still the chosen candidate");
        assert_ne!((out, sel), (1, 1), "and NOT the stale indices");

        assert!(relocate_detail("vanished", &s.agents).is_none());
        assert!(rebind_picker_sel_by_label(0, Some("vanished"), &picker).is_none());
    }

    /// A failed swap keeps the user on the page WITH the reason,
    /// matching promote: a silently unchanged roster reads as "clank
    /// ignored me".
    #[test]
    fn apply_swap_failure_surfaces_the_error_and_stays() {
        let repo = detail_repo();
        let mut s = two_agent_snap();
        // Swapping IN a label already on the roster is refused by the
        // core.
        let picker = vec![crate::cli::status::AvailableAgent {
            label: "claude".to_string(),
            tool: "claude".to_string(),
            invocation: "claude".to_string(),
            description: None,
        }];
        let home = library_home(&["claude"]);
        let mut err = None;
        let mode = apply_swap(
            1,
            0,
            &mut s,
            &picker,
            repo.path(),
            Some(home.path()),
            &mut err,
        );
        assert_eq!(
            mode,
            Mode::AgentDetail { idx: 1, sel: 0 },
            "stays on the page"
        );
        assert!(err.is_some(), "and says why");
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
            1,
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
        let next = apply_detail_action(
            DetailAction::Remove,
            1,
            3,
            &mut s,
            repo.path(),
            &mut None,
            1,
        );
        assert_eq!(
            next,
            Mode::Confirm {
                action: ConfirmAction::RemoveAgent { idx: 1 }
            }
        );
    }
}
