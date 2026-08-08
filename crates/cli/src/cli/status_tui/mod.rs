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

pub(crate) mod input;
use input::*;

mod render;
use render::*;

mod scroll;
use scroll::*;

mod zellij;
use zellij::TabIndicator;

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
/// e4bccc5). The mutated `.clank/config.json` is local-only repo
/// state; the confirm modal names that local write explicitly.
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

/// Retained-set retargeting (tui-github-event-page): follow the
/// FIRST retained key still present in the fresh events (first-key
/// wins across component splits), refresh the retained set from the
/// newly targeted component, close only when none survive. Pure over
/// the fresh event list — the retarget contract's test seam.
fn refetch_event_page(
    page: &mut Option<EventPage>,
    events: &[crate::cli::github_timeline::MergedEvent],
) -> bool {
    let Some(p) = page.as_mut() else {
        return false;
    };
    for key in &p.retained {
        if let Some(ev) = events
            .iter()
            .find(|e| e.members.iter().any(|m| &m.key == key))
        {
            p.target = key.clone();
            p.retained = ev.members.iter().map(|m| m.key.clone()).collect();
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
    event: &crate::cli::github_timeline::MergedEvent,
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

fn roster_confirm_after_decision(
    action: ConfirmAction,
    roster_write_succeeded: bool,
    picker_len: usize,
    agents_len: usize,
) -> (Mode, bool) {
    if roster_write_succeeded {
        return (Mode::AgentPanel { sel: agents_len }, true);
    }
    match action {
        ConfirmAction::AddCandidate { idx } if idx < picker_len => {
            (Mode::AddPicker { sel: idx }, false)
        }
        ConfirmAction::RemoveAgent { idx } if idx < agents_len => {
            (Mode::AgentDetail { idx, sel: 0 }, false)
        }
        _ => (Mode::AgentPanel { sel: agents_len }, true),
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

fn rebind_add_picker_after_refresh(
    old_sel: usize,
    label: Option<&str>,
    picker: &[crate::cli::status::AvailableAgent],
    agents_len: usize,
) -> Mode {
    match rebind_picker_sel_by_label(old_sel, label, picker) {
        Some(sel) => Mode::AddPicker { sel },
        None => Mode::AgentPanel { sel: agents_len },
    }
}

fn rebind_add_confirm_after_refresh(
    old_idx: usize,
    label: Option<&str>,
    picker: &[crate::cli::status::AvailableAgent],
    agents_len: usize,
) -> Mode {
    let rebound = if let Some(label) = label {
        picker.iter().position(|c| c.label.as_str() == label)
    } else {
        (old_idx < picker.len()).then_some(old_idx)
    };
    match rebound {
        Some(idx) => Mode::Confirm {
            action: ConfirmAction::AddCandidate { idx },
        },
        None => Mode::AgentPanel { sel: agents_len },
    }
}

fn rebind_remove_confirm_after_refresh(
    label: Option<&str>,
    agents: &[crate::cli::status::AgentAutoRow],
    agents_len: usize,
) -> Mode {
    match label.and_then(|l| relocate_detail(l, agents)) {
        Some(idx) => Mode::Confirm {
            action: ConfirmAction::RemoveAgent { idx },
        },
        None => Mode::AgentPanel { sel: agents_len },
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

    // Declared BEFORE the alt-screen guard: reverse drop order joins
    // the worker AFTER the terminal is restored, on every exit path
    // (tui-reconcile-off-loop).
    let reconcile_worker = zellij::ReconcileWorker::spawn(repo.clone());

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
    // The open EVENT page (tui-github-event-page); invariant: `Some`
    // ⟺ mode is `EventDetail` (set/cleared together, like plan_page).
    let mut event_page: Option<EventPage> = None;
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
            event_page: event_page.as_ref(),
            plan_input: plan_input.as_ref(),
            picker: &picker,
            log_cursor: log.cursor,
            lift: 0,
        };
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
                log.window += (rows as usize).max(1);
                let before = snapshot.log_rows.len();
                snapshot.log_rows = crate::cli::status::tui_log_rows(&repo, log.window).await;
                if snapshot.log_rows.len() == before {
                    log.complete = true; // hit the root — stop growing
                }
            }
            log.fill = false;
        }

        let total = head + snapshot.log_rows.len();
        // Pressure lift (tui-short-pane-whole-scroll): in a short pane a
        // focused log keeps a working viewport by lifting scrollable
        // header rows off the top, growing with the cursor's descent so
        // the header restores as the selection walks back up. Derived
        // from the TRUE header length (a clipped render reports
        // capacity 0 however deep the header overflows — codex be2b054)
        // and the POST-FILL total (the policy clamps the cursor exactly
        // as settle will). Capacity comes from the SAME budget function
        // the render draws, so no probe render is needed.
        let header_len = render::scrollable_header(&snapshot, rows, cols, frame, &view).len();
        let lift = pressure_lift(log_focused, rows as usize, header_len, log.cursor, total);
        let capacity = log_budget(
            rows as usize,
            header_len,
            lift,
            !snapshot.agents.is_empty(),
            log_focused,
            total,
        )
        .capacity;
        // Derive the viewport from the cursor (focused) or keep the last
        // page full (unfocused) — see [`LogView::settle`].
        log.settle(capacity, total, log_focused);
        // Rebuild the view with the clamped cursor/offset for the paint.
        let view = PanelView { lift, ..view };
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
                            Key::Escape | Key::Focus | Key::Char(b'a') => {
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
                        Mode::PlanInput { .. } => {}
                        // The plan-actions page (tui-plan-actions-page).
                        Mode::EventDetail { sel } => {
                            let Some(ep) = event_page.clone() else {
                                mode = Mode::LogScroll; // invariant breach — bail out
                                continue;
                            };
                            let actions = event_actions(ep.event.url.is_some(), ep.event.unhandled);
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
                                    mode = Mode::LogScroll;
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
                                        let _ = crate::cli::events::ack_members(&repo, &keys);
                                        refresh_pending = true;
                                    }
                                    EventEffect::Close => {
                                        event_page = None;
                                        mode = Mode::LogScroll;
                                    }
                                    EventEffect::None => {}
                                },
                                EventNav::None => {}
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
                            Key::Focus | Key::Char(b'a') => {
                                mode = mode.toggle_focus(snapshot.agents.len())
                            }
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
                                    if let Some(ev) = snapshot.github_events.get(*event_idx)
                                        && let Some(first) = ev.members.first()
                                    {
                                        event_page = Some(EventPage {
                                            target: first.key.clone(),
                                            retained: ev
                                                .members
                                                .iter()
                                                .map(|m| m.key.clone())
                                                .collect(),
                                            event: ev.clone(),
                                            prompts: event_prompts(&repo, ev),
                                            scroll: 0,
                                        });
                                        mode = Mode::EventDetail { sel: 0 };
                                    }
                                    continue;
                                }
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
                // Capture index-addressed targets by LABEL from the OLD
                // state before rebuilding — external roster/library edits
                // can reorder rows, so kept indices could silently retarget
                // different agents/candidates. We re-locate the same labels
                // below.
                let detail_label = match mode {
                    Mode::AgentDetail { idx, .. }
                    | Mode::Confirm {
                        action: ConfirmAction::RemoveAgent { idx },
                    } => snapshot.agents.get(idx).map(|a| a.label.clone()),
                    _ => None,
                };
                let picker_label = match mode {
                    Mode::AddPicker { sel }
                    | Mode::Confirm {
                        action: ConfirmAction::AddCandidate { idx: sel },
                    } => picker.get(sel).map(|c| c.label.clone()),
                    _ => None,
                };
                let keep_picker = matches!(
                    mode,
                    Mode::AddPicker { .. }
                        | Mode::Confirm {
                            action: ConfirmAction::AddCandidate { .. }
                        }
                );
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
                // A FAILED rebuild must not kill the TUI (a gix
                // status walk racing a worktree mutation is routine)
                // and must not advance the accepted signature — see
                // [`apply_refresh`].
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
                let (fresh_rows, fresh_events) =
                    crate::cli::status::tui_log_with_events(&repo, log.window).await;
                let refresh_ok =
                    apply_refresh(&mut snapshot, fresh_rows, rebuilt, &mut refresh_failures);
                // Rows and events must come from the SAME read —
                // event_idx targets this list (set after apply so a
                // replaced snapshot's own build-time read never
                // misaligns them; the kept-frame arm needs them too).
                snapshot.github_events = fresh_events;
                if refresh_ok {
                    last_sig = sig;
                    refresh_retry_at = None;
                } else {
                    refresh_retry_at =
                        Some(std::time::Instant::now() + retry_delay(refresh_failures));
                }
                reconcile_worker.observe(&snapshot);
                log.complete = false;
                log.request_fill();
                if keep_picker {
                    picker =
                        crate::cli::status::available_agents(home.as_deref(), &snapshot.agents);
                } else {
                    picker.clear();
                }
                mode = match mode {
                    Mode::LogScroll => Mode::LogScroll,
                    // The plan page doesn't depend on the roster — it
                    // must survive an empty-roster refresh (its arms are
                    // below); everything agent-shaped drops to the log.
                    Mode::AgentPanel { .. } | Mode::AgentDetail { .. }
                        if snapshot.agents.is_empty() =>
                    {
                        Mode::LogScroll
                    }
                    Mode::Confirm {
                        action: ConfirmAction::RemoveAgent { .. },
                    } if snapshot.agents.is_empty() => Mode::LogScroll,
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
                    // The add picker and add confirm track ONE global-library
                    // candidate by identity: if the selected candidate is
                    // still available after a config refresh, keep the page;
                    // otherwise close to the panel.
                    Mode::AddPicker { sel } => rebind_add_picker_after_refresh(
                        sel,
                        picker_label.as_deref(),
                        &picker,
                        snapshot.agents.len(),
                    ),
                    Mode::Confirm {
                        action: ConfirmAction::AddCandidate { idx },
                    } => rebind_add_confirm_after_refresh(
                        idx,
                        picker_label.as_deref(),
                        &picker,
                        snapshot.agents.len(),
                    ),
                    // Remove confirms also track the agent by identity, so a
                    // tier change/reorder cannot make "yes" remove the wrong
                    // row. If the target vanished, close to the panel.
                    Mode::Confirm {
                        action: ConfirmAction::RemoveAgent { .. },
                    } => rebind_remove_confirm_after_refresh(
                        detail_label.as_deref(),
                        &snapshot.agents,
                        snapshot.agents.len(),
                    ),
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
                        if refetch_event_page(&mut event_page, &snapshot.github_events) {
                            if let Some(ep) = event_page.as_mut() {
                                ep.prompts = event_prompts(&repo, &ep.event.clone());
                            }
                            Mode::EventDetail { sel }
                        } else {
                            Mode::LogScroll
                        }
                    }
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
                    Mode::Confirm { .. } => Mode::AgentPanel {
                        sel: snapshot.agents.len(),
                    },
                };
                if let Some(tab) = tab.as_mut() {
                    tab.update(&bar_emoji(&snapshot));
                }
                // Retitles are the worker's: derive pure glyph data
                // here, send, never touch zellij on the loop
                // (tui-reconcile-off-loop).
                reconcile_worker.update_glyphs(&snapshot);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    fn mk_member(
        agent: &str,
        seq: u64,
        ident: &str,
        acked: bool,
    ) -> crate::cli::github_timeline::MemberRef {
        crate::cli::github_timeline::MemberRef {
            key: crate::cli::github_timeline::MemberKey {
                agent: agent.into(),
                source: "github-o-r-aaaa".into(),
                seq,
                ident: ident.into(),
            },
            acked,
            transport: crate::cli::github_event_log::Transport::Poll,
        }
    }

    fn mk_event(
        members: Vec<crate::cli::github_timeline::MemberRef>,
    ) -> crate::cli::github_timeline::MergedEvent {
        crate::cli::github_timeline::MergedEvent {
            at: 1,
            repo: "o/r".into(),
            event: "issue_opened".into(),
            detail: None,
            number: None,
            title: None,
            actor: None,
            url: None,
            seen_by: members.iter().map(|m| m.key.agent.clone()).collect(),
            unhandled: members.iter().any(|m| !m.acked),
            baseline: false,
            members,
        }
    }

    #[test]
    fn event_action_effects_record_the_exact_url_and_fanout() {
        // codex 525cef1: the EXECUTED step is the seam — the effect
        // carries the exact URL production hands the shared opener,
        // and the exact unhandled keys the fanout receives; a no-URL
        // browser action resolves to None (belt: the menu omits it).
        let handled = crate::cli::github_timeline::MemberRef {
            acked: true,
            ..mk_member("beta", 2, "f:2", true)
        };
        let mut ev = mk_event(vec![mk_member("alpha", 1, "f:1", false), handled]);
        ev.url = Some("https://github.com/o/r/pull/12".into());
        let ep = EventPage {
            target: ev.members[0].key.clone(),
            retained: ev.members.iter().map(|m| m.key.clone()).collect(),
            event: ev,
            prompts: Vec::new(),
            scroll: 0,
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
    fn event_page_retargets_through_the_retained_set() {
        // The retarget contract (tui-github-event-page): first
        // surviving retained key wins — including across a component
        // SPLIT — the retained set refreshes from the new component,
        // and the page closes only when nothing survives.
        let a = mk_member("alpha", 1, "f:1", false);
        let b = mk_member("beta", 1, "f:1", false);
        let mut page = Some(EventPage {
            target: a.key.clone(),
            retained: vec![a.key.clone(), b.key.clone()],
            event: mk_event(vec![a.clone(), b.clone()]),
            prompts: Vec::new(),
            scroll: 0,
        });

        // Target's copy vanished; beta's survives in a fresh component.
        let fresh = vec![mk_event(vec![b.clone()])];
        assert!(refetch_event_page(&mut page, &fresh));
        let p = page.as_ref().unwrap();
        assert_eq!(p.target, b.key, "retargeted to the first surviving key");
        assert_eq!(p.retained, vec![b.key.clone()], "retained set refreshed");

        // SPLIT: alpha and beta now live in SEPARATE components; a
        // page retaining both follows the FIRST retained key's side.
        let mut split_page = Some(EventPage {
            target: a.key.clone(),
            retained: vec![a.key.clone(), b.key.clone()],
            event: mk_event(vec![a.clone(), b.clone()]),
            prompts: Vec::new(),
            scroll: 0,
        });
        let split = vec![mk_event(vec![b.clone()]), mk_event(vec![a.clone()])];
        assert!(refetch_event_page(&mut split_page, &split));
        let p = split_page.as_ref().unwrap();
        assert_eq!(p.target, a.key, "first-key wins picks alpha's side");
        assert_eq!(p.retained, vec![a.key.clone()]);

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

    #[test]
    fn apply_refresh_keeps_frame_and_signature_on_failure() {
        // THE contract (intro 1913796): the accepted signature
        // advances ONLY on successful snapshot replacement — a
        // failed rebuild returns false (caller leaves last_sig
        // untouched, so the SAME signature stays retryable), keeps
        // the old frame, and mounts exactly one transient notice.
        use crate::cli::log::OnelineRow;
        use crate::cli::status_tui::fixtures::snap;
        let mut current = snap(vec![], vec![]);
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
        let mut next = snap(vec![], vec![]);
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
    fn roster_confirm_decision_returns_to_origin_pages() {
        assert_eq!(
            roster_confirm_after_decision(ConfirmAction::AddCandidate { idx: 1 }, false, 3, 2),
            (Mode::AddPicker { sel: 1 }, false),
            "cancel/failed add returns to the selected picker candidate"
        );
        assert_eq!(
            roster_confirm_after_decision(ConfirmAction::RemoveAgent { idx: 1 }, false, 0, 2),
            (Mode::AgentDetail { idx: 1, sel: 0 }, false),
            "cancel/failed remove returns to the target detail page"
        );
        assert_eq!(
            roster_confirm_after_decision(ConfirmAction::AddCandidate { idx: 4 }, false, 2, 2),
            (Mode::AgentPanel { sel: 2 }, true),
            "an invalid add target falls back to the panel and drops picker state"
        );
        assert_eq!(
            roster_confirm_after_decision(ConfirmAction::RemoveAgent { idx: 1 }, true, 0, 2),
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
        assert_eq!(
            rebind_add_confirm_after_refresh(0, Some("grok"), &picker, 2),
            Mode::Confirm {
                action: ConfirmAction::AddCandidate { idx: 1 }
            },
            "add confirm follows the same candidate after the picker reorders"
        );
        assert_eq!(
            rebind_add_confirm_after_refresh(0, Some("missing"), &picker, 2),
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
            ConfirmAction::AddCandidate { idx: 0 },
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
