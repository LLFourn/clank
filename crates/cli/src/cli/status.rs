//! `clank status` — read-only summary of the current repo's Clank
//! state. Folds the repo, projects per-plan via `clank-core::plan_view`,
//! then renders. Never blocks.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use super::{StatusArgs, repo_basename, resolve_repo};
use crate::cli::plan_resolve::parse_arg;
use crate::lifecycle::PlanKey;
use crate::repo_state::RepoState;
use clank_core::plan_view::WaitingOn;
use clank_core::repo_state::FinishedPlan;
use clank_core::vocab::CommitGateState;
use clank_core::wait::PlanWorkState;

pub struct StatusSnapshot {
    pub(crate) repo_path: PathBuf,
    pub(crate) basename: String,
    pub(crate) branch: Option<String>,
    pub(crate) head_sha: Option<String>,
    pub(crate) head_subject: Option<String>,
    pub(crate) worktree_dirty: bool,
    pub(crate) plans: Vec<PlanWorkState>,
    pub(crate) last_finished: Option<FinishedPlan>,
    pub(crate) blocks: Vec<crate::cli::block::BlockEntry>,
    /// Queued plan names in priority order (the TUI's tier-4 list;
    /// counts everywhere else derive from it). Plan:
    /// clank-status-tui.
    pub(crate) queue: Vec<String>,
    /// The team's master label, when a team resolves (render path:
    /// degrades to None on a teamless repo). The TUI's idle+queue
    /// headline names master as the agent whose turn it is.
    pub(crate) master: Option<String>,
    /// Shelved plans (stem, what they're waiting for if `--for`
    /// was used, and whether that wait is over). Plan:
    /// plan-lifecycle-verbs.
    pub(crate) shelved: Vec<ShelvedView>,
    /// Recent activity as STRUCTURED oneline rows (umbrella
    /// headers + commits + reviews), chronological (oldest first)
    /// — the TUI's log pane takes the tail and styles per row
    /// kind. Built from the fold's LogEvents (subjects carried; no
    /// per-event git shelling). NOT emitted by `to_json` — the
    /// status --json log shape is unchanged; `clank log --json`
    /// is the machine log surface (log-plan-umbrellas, ruthless
    /// 3ea8580 concern 1). Plans: status-tui-live-log,
    /// log-plan-umbrellas.
    pub(crate) log_rows: Vec<crate::cli::log::OnelineRow>,
}

/// One shelved plan as the renderers see it.
pub(crate) struct ShelvedView {
    pub(crate) stem: String,
    pub(crate) waiting_for: Option<String>,
    /// True when `waiting_for` names a plan that has finished —
    /// the unshelve nudge.
    pub(crate) ready: bool,
}

/// In-process convenience for tests / callers that want a status
/// snapshot from just `(repo, home)`: cache enabled, no plan
/// filter, not watch mode. Hides `CachePolicy`/basename plumbing.
/// Plan: dogfood-init-setup-in-tests (Phase B).
pub async fn snapshot(repo: &Path, home: Option<&Path>) -> anyhow::Result<StatusSnapshot> {
    let basename = repo_basename(repo)?;
    StatusSnapshot::build_async(
        repo,
        &basename,
        home,
        crate::rebuild::CachePolicy::Use,
        None,
        false,
    )
    .await
}

impl StatusSnapshot {
    /// Build the status snapshot. `home` is explicit (not read
    /// from `$HOME`) so in-process callers — tests + the `clank
    /// status` shell — control which user-scope config layers in.
    /// Plan: dogfood-init-setup-in-tests (Phase B).
    pub async fn build_async(
        repo: &Path,
        basename: &str,
        home: Option<&Path>,
        policy: crate::rebuild::CachePolicy,
        plan_arg: Option<&str>,
        watch_mode: bool,
    ) -> anyhow::Result<Self> {
        let state = crate::rebuild::rebuild_repo_with_policy(repo, policy)
            .await
            .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
        let log_rows = recent_log_rows(repo, &state).await;
        Self::from_state(repo, basename, home, &state, plan_arg, watch_mode, log_rows)
    }

    fn from_state(
        repo: &Path,
        basename: &str,
        home: Option<&Path>,
        state: &RepoState,
        plan_arg: Option<&str>,
        watch_mode: bool,
        log_rows: Vec<crate::cli::log::OnelineRow>,
    ) -> anyhow::Result<Self> {
        let (branch, head_sha, head_subject) = head_info(repo);
        let worktree_dirty = worktree_dirty(repo)?;

        let config = crate::cli::config::load_with_home(repo, home);
        // Plan: teams-based-agent-registration. `status` is a
        // read-only renderer (also reused by `clank html`), so it
        // DEGRADES on a team-less / misconfigured repo: no team →
        // empty reviewer tiers → gate computes as zero-reviewer
        // (Approved). It never hard-errors the way the
        // workflow-driving commands (wfw / finish / promote) do.
        let registered = crate::agent_store::try_resolve_via_team_with(repo, home)
            .ok()
            .flatten();
        let master = registered
            .as_ref()
            .map(|set| set.master.as_str().to_string());
        let (commit_reviewers, gate_reviewers) = registered
            .map(|set| {
                (
                    set.commit_reviewers
                        .into_iter()
                        .map(|a| a.label)
                        .collect::<Vec<_>>(),
                    set.gate_reviewers
                        .into_iter()
                        .map(|a| a.label)
                        .collect::<Vec<_>>(),
                )
            })
            .unwrap_or_default();
        let work_policy = clank_core::wait::WorkPolicy {
            plan_feedback: config.review.plan_feedback,
            adhoc_feedback: config.review.adhoc_feedback,
            commit_reviewers,
            gate_reviewers,
        };
        let reviews =
            crate::fs_plan_state_lookup::FsPlanStateLookup::new(repo, state.head.as_ref());
        let work_status = state.fold.derive_status(&reviews, &work_policy);

        let (plans, last_finished) = select_plans_and_finished(
            &work_status.plans,
            &state.fold,
            basename,
            plan_arg,
            watch_mode,
        )?;

        let blocks = crate::cli::block::scan_blocks(repo);
        // scan_queue returns entries sorted by (priority, name);
        // capture the names so the snapshot stays the single source
        // of truth for the queue (count derives from it).
        let queue: Vec<String> = crate::cli::queue::scan_queue(repo)
            .into_iter()
            .map(|e| e.name)
            .collect();

        let shelved: Vec<ShelvedView> = crate::cli::shelve::scan_shelved(repo)
            .into_iter()
            .map(|(stem, st)| {
                let ready = st.waiting_for.as_deref().is_some_and(|w| {
                    state
                        .fold
                        .finished_plans
                        .iter()
                        .any(|fp| fp.plan.as_str() == w)
                });
                ShelvedView {
                    stem,
                    waiting_for: st.waiting_for,
                    ready,
                }
            })
            .collect();

        Ok(Self {
            repo_path: repo.to_path_buf(),
            basename: basename.to_string(),
            branch,
            head_sha,
            head_subject,
            worktree_dirty,
            plans,
            last_finished,
            blocks,
            queue,
            master,
            shelved,
            log_rows,
        })
    }

    /// Test accessor for the TUI log lines (integration tests live
    /// in a separate crate; the field stays crate-private).
    pub fn log_lines_for_test(&self) -> Vec<String> {
        crate::cli::log::oneline_plain_lines(&self.log_rows)
    }

    pub fn to_json(&self) -> serde_json::Value {
        let plans: Vec<serde_json::Value> = self
            .plans
            .iter()
            .map(|v| {
                serde_json::json!({
                    "plan": v.plan.as_str(),
                    // None when plan is blocked with no reviewable commit yet.
                    "latest_reviewable_sha": v.sha.as_ref().map(|s| s.as_str()),
                    "gate_state": v.gate,
                    // Structured `WaitingOn` via existing serde derive
                    // (was a `Debug` string — never a stable contract).
                    // Per status-blocks-dominate-gate.
                    "waiting_on": v.waiting_on,
                })
            })
            .collect();

        let finished: Vec<serde_json::Value> = if self.plans.is_empty() {
            self.last_finished
                .as_ref()
                .map(|fp| {
                    vec![serde_json::json!({
                        "plan": fp.plan.as_str(),
                        "intro": fp.intro.as_str(),
                        "finalized_at": fp.finalized_at.as_str(),
                    })]
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let all_blocks: Vec<serde_json::Value> = self
            .blocks
            .iter()
            .map(|b| {
                serde_json::json!({
                    "agent": b.agent,
                    "name": b.name,
                    "plan": b.plan,
                    "question": b.question,
                    "answer": b.answer,
                    "pending": b.answer.is_none(),
                })
            })
            .collect();

        let mut obj = serde_json::json!({
            "repo_basename": self.basename,
            "branch": self.branch,
            "head_sha": self.head_sha,
            "head_subject": self.head_subject,
            "worktree_dirty": self.worktree_dirty,
            "plans": plans,
            "finished_plans": finished,
            "blocks": all_blocks,
        });
        if !self.queue.is_empty() {
            obj["queue_count"] = serde_json::json!(self.queue.len());
            // Names in priority order. Additive wire-format change
            // (clank-status-tui); queue_count kept for consumers.
            obj["queue"] = serde_json::json!(self.queue);
        }
        if !self.shelved.is_empty() {
            // Additive (plan-lifecycle-verbs).
            obj["shelved"] = serde_json::json!(
                self.shelved
                    .iter()
                    .map(|sv| {
                        serde_json::json!({
                            "plan": sv.stem,
                            "waiting_for": sv.waiting_for,
                            "ready": sv.ready,
                        })
                    })
                    .collect::<Vec<_>>()
            );
        }
        obj
    }

    pub fn to_human(&self) -> String {
        let mut out = String::new();
        use std::fmt::Write as _;

        let _ = writeln!(out, "repo:   {}", self.repo_path.display());
        if let Some(b) = &self.branch {
            let _ = writeln!(out, "branch: {b}");
        }
        if let Some(s) = &self.head_sha {
            let _ = writeln!(out, "head:   {s}");
        }
        let _ = writeln!(
            out,
            "dirty:  {}",
            if self.worktree_dirty { "yes" } else { "no" }
        );
        if !self.queue.is_empty() {
            let _ = writeln!(
                out,
                "queue:  {} item{}",
                self.queue.len(),
                if self.queue.len() == 1 { "" } else { "s" }
            );
        }
        for sv in &self.shelved {
            let note = match (&sv.waiting_for, sv.ready) {
                (Some(w), true) => format!(" (was waiting on {w} — FINISHED; unshelve?)"),
                (Some(w), false) => format!(" (waiting on {w})"),
                (None, _) => String::new(),
            };
            let _ = writeln!(out, "shelved: {}{note}", sv.stem);
        }

        if self.plans.is_empty() && self.last_finished.is_none() {
            let _ = writeln!(out);
            let _ = writeln!(out, "no active plan, nothing pending");
        }

        for v in &self.plans {
            let _ = writeln!(out);
            let _ = writeln!(out, "plan: {}", v.plan.as_str());
            // Omit `latest reviewable:` line when sha is None (blocked
            // plan with no reviewable commit yet — e.g. intro-only).
            if let Some(sha) = &v.sha {
                let _ = writeln!(out, "  latest reviewable: {}", short_sha(sha.as_str()));
            }
            // `BLOCKED` uppercase on the gate line is a renderer-only
            // emphasis for the blocked state per lloyd's "screaming
            // loudly" directive. Other gates stay lowercase via
            // CommitGateState's Display impl. Wire form
            // (CommitGateState::as_str) remains lowercase "blocked".
            let gate_display = if matches!(v.gate, CommitGateState::Blocked) {
                "BLOCKED".to_string()
            } else {
                v.gate.to_string()
            };
            let _ = writeln!(out, "  gate:              {gate_display}");
            let _ = writeln!(out, "  waiting on:        {}", waiting_actor(&v.waiting_on));
            let _ = writeln!(
                out,
                "  reason:            {}",
                waiting_reason(&v.waiting_on)
            );
        }

        if self.plans.is_empty() {
            if let Some(fp) = &self.last_finished {
                let _ = writeln!(out);
                let _ = writeln!(
                    out,
                    "last finished: {} (finalized {})",
                    fp.plan.as_str(),
                    short_sha(fp.finalized_at.as_str())
                );
            }
        }

        if !self.blocks.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(out, "blocks:");
            for b in &self.blocks {
                let scope = b.plan.as_deref().unwrap_or("repo");
                if let Some(ref answer) = b.answer {
                    let _ = writeln!(
                        out,
                        "  UNBLOCKED ({}, scope: {}): {}",
                        b.agent, scope, b.question
                    );
                    let _ = writeln!(out, "    answer: {answer}");
                } else {
                    let _ = writeln!(
                        out,
                        "  BLOCKED ({}, scope: {}): {}",
                        b.agent, scope, b.question
                    );
                }
            }
        }

        out.trim_end_matches('\n').to_string()
    }

    fn to_json_compact(&self) -> String {
        serde_json::to_string(&self.to_json()).unwrap_or_default()
    }
}

pub async fn run(args: StatusArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;
    let policy = if args.no_cache {
        crate::rebuild::CachePolicy::Bypass
    } else {
        crate::rebuild::CachePolicy::Use
    };

    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);

    if args.tui {
        return crate::cli::status_tui::run_tui(repo, basename, home, policy).await;
    }

    if args.watch {
        return run_watch(
            repo,
            basename,
            home,
            policy,
            args.json,
            args.plan.as_deref(),
        )
        .await;
    }

    let snapshot = StatusSnapshot::build_async(
        &repo,
        &basename,
        home.as_deref(),
        policy,
        args.plan.as_deref(),
        false,
    )
    .await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&snapshot.to_json())?);
    } else {
        print!("{}", snapshot.to_human());
        println!();
    }
    Ok(())
}

async fn run_watch(
    repo: std::path::PathBuf,
    basename: String,
    home: Option<std::path::PathBuf>,
    policy: crate::rebuild::CachePolicy,
    json: bool,
    plan_arg: Option<&str>,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<()>();
    let mut watcher = build_watcher(tx)?;
    attach_watcher(&mut watcher, &repo)?;

    let mut last_emitted: Option<String> = None;

    loop {
        let snapshot =
            StatusSnapshot::build_async(&repo, &basename, home.as_deref(), policy, plan_arg, true)
                .await?;

        let output = if json {
            snapshot.to_json_compact()
        } else {
            snapshot.to_human()
        };

        if last_emitted.as_deref() != Some(&output) {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            if last_emitted.is_some() {
                writeln!(out)?;
            }
            writeln!(out, "{output}")?;
            out.flush()?;
            last_emitted = Some(output);
        }

        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(()) => while rx.recv_timeout(Duration::from_millis(200)).is_ok() {},
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("filesystem watcher disconnected")
            }
        }
    }
}

/// Recent activity for the TUI's log pane: a bounded range-fold
/// (last `LOG_WINDOW` commits; gix-backed reads, no per-event
/// subprocess — ruthless 54c37f6 concern 1) rendered through the
/// shared `clank log --oneline` row producer to PLAIN lines (the
/// TUI styles them). Scope: the single active plan's events when
/// exactly one plan is active, repo-wide otherwise. Best-effort:
/// any failure yields an empty pane, never a status error.
async fn recent_log_rows(repo: &Path, state: &RepoState) -> Vec<crate::cli::log::OnelineRow> {
    use clank_core::repo_state::LogEvent;
    const LOG_WINDOW: usize = 30;

    let Some(head) = state.head.clone() else {
        return Vec::new();
    };
    let from = crate::cli::log::git_rev_parse(repo, &format!("HEAD~{LOG_WINDOW}"));
    let Ok((_state, events)) = crate::rebuild::rebuild_from(repo, from.as_ref(), &head).await
    else {
        return Vec::new();
    };

    let active: Vec<&PlanKey> = state.fold.plans.keys().collect();
    let plan_filter: Option<&PlanKey> = match active.as_slice() {
        [only] => Some(only),
        _ => None,
    };
    let filtered: Vec<&LogEvent> = events
        .iter()
        .filter(|e| match e {
            LogEvent::AdHoc { .. } => plan_filter.is_none(),
            LogEvent::PlanIntro { plan, .. }
            | LogEvent::PlanCommit { plan, .. }
            | LogEvent::PlanFinalized { plan, .. }
            | LogEvent::PlanDeleted { plan, .. } => plan_filter.is_none_or(|f| f == plan),
        })
        .collect();

    let reviewable: Vec<crate::lifecycle::CommitSha> = filtered
        .iter()
        .filter_map(|e| match e {
            LogEvent::PlanCommit { sha, .. }
            | LogEvent::PlanIntro { sha, .. }
            | LogEvent::AdHoc { sha, .. } => Some(sha.clone()),
            _ => None,
        })
        .collect();
    let reviews = crate::cli::log::collect_reviews(repo, &reviewable);
    crate::cli::log::oneline_rows(&filtered, &reviews)
}

pub(crate) fn build_watcher(tx: mpsc::Sender<()>) -> anyhow::Result<RecommendedWatcher> {
    Ok(notify::recommended_watcher(
        move |res: notify::Result<notify::Event>| {
            if res.is_ok() {
                let _ = tx.send(());
            }
        },
    )?)
}

pub(crate) fn attach_watcher(watcher: &mut RecommendedWatcher, repo: &Path) -> anyhow::Result<()> {
    let clank_root = repo.join(".clank");
    if let Err(e) = std::fs::create_dir_all(&clank_root) {
        anyhow::bail!("ensure `{}` exists: {e}", clank_root.display());
    }
    watcher
        .watch(&clank_root, RecursiveMode::Recursive)
        .map_err(|e| anyhow::anyhow!("watch `{}` failed: {e}", clank_root.display()))?;

    let git_dir = git_resolve_dir(repo)?;
    watcher
        .watch(&git_dir, RecursiveMode::Recursive)
        .map_err(|e| anyhow::anyhow!("watch `{}` failed: {e}", git_dir.display()))?;
    Ok(())
}

fn git_resolve_dir(repo: &Path) -> anyhow::Result<std::path::PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--git-dir"])
        .output()
        .map_err(|e| anyhow::anyhow!("git rev-parse --git-dir: {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "git rev-parse --git-dir failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let p = std::path::PathBuf::from(&raw);
    let absolute = if p.is_absolute() { p } else { repo.join(p) };
    Ok(dunce::canonicalize(&absolute).unwrap_or(absolute))
}

fn select_plans_and_finished(
    work_plans: &[PlanWorkState],
    fold: &clank_core::repo_state::RepoState,
    basename: &str,
    plan_arg: Option<&str>,
    watch_mode: bool,
) -> anyhow::Result<(Vec<PlanWorkState>, Option<FinishedPlan>)> {
    if let Some(raw) = plan_arg {
        let stem = parse_arg(raw, basename)?;
        let key = PlanKey::parse(&stem)
            .map_err(|e| anyhow::anyhow!("invalid plan stem `{stem}`: {e}"))?;

        let matching: Vec<PlanWorkState> = work_plans
            .iter()
            .filter(|ps| ps.plan == key)
            .cloned()
            .collect();

        if !matching.is_empty() {
            return Ok((matching, None));
        }

        if watch_mode {
            let last_finished = fold
                .finished_plans
                .iter()
                .rev()
                .find(|fp| fp.plan == key)
                .cloned();
            if last_finished.is_some() {
                return Ok((vec![], last_finished));
            }
        }

        anyhow::bail!(
            "plan `{basename}/{stem}.md` not active. active: {}",
            active_summary(fold, basename)
        );
    }

    let plans = work_plans.to_vec();
    let last_finished = if plans.is_empty() {
        fold.finished_plans.last().cloned()
    } else {
        None
    };
    Ok((plans, last_finished))
}

pub(crate) fn waiting_actor(w: &WaitingOn) -> String {
    match w {
        WaitingOn::Blocked { block } => block.creator.as_str().to_string(),
        WaitingOn::ReviewerApprovalsMissing { missing }
        | WaitingOn::GateReviewersMissing { missing } => missing
            .iter()
            .map(|a| a.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        WaitingOn::MasterToRevise { .. }
        | WaitingOn::MasterToContinue
        | WaitingOn::MasterToFinalize
        | WaitingOn::MasterToCommit => "master".into(),
    }
}

/// Max chars of block-message body shown on the per-plan `reason:`
/// line. Full message stays in the bottom `blocks:` footer.
const BLOCK_REASON_MAX_LEN: usize = 80;

/// First line of a block message, trimmed + truncated to
/// `BLOCK_REASON_MAX_LEN` chars + `…` ellipsis if longer.
fn first_line(s: &str) -> String {
    let line = s.trim_start();
    let line = line.split('\n').next().unwrap_or("");
    if line.chars().count() <= BLOCK_REASON_MAX_LEN {
        line.to_string()
    } else {
        let truncated: String = line.chars().take(BLOCK_REASON_MAX_LEN).collect();
        format!("{truncated}…")
    }
}

pub(crate) fn waiting_reason(w: &WaitingOn) -> String {
    match w {
        WaitingOn::Blocked { block } => format!("blocked: {}", first_line(&block.message)),
        WaitingOn::ReviewerApprovalsMissing { missing } => {
            let names = missing
                .iter()
                .map(|a| a.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("missing approval from {names}")
        }
        WaitingOn::GateReviewersMissing { missing } => {
            let names = missing
                .iter()
                .map(|a| a.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("commit-tier reviewers approved; waiting on gate-tier {names}")
        }
        WaitingOn::MasterToRevise {
            requesters,
            ambiguous,
        } => {
            let mut parts = Vec::new();
            if !requesters.is_empty() {
                let n = requesters
                    .iter()
                    .map(|a| a.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                parts.push(format!("changes requested by {n}"));
            }
            if !ambiguous.is_empty() {
                let n = ambiguous
                    .iter()
                    .map(|a| a.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                parts.push(format!("ambiguous verdict from {n}"));
            }
            parts.join("; ")
        }
        WaitingOn::MasterToContinue => {
            "gate approved (not FINISHED) — continue work or ask a reviewer to mark FINISHED".into()
        }
        WaitingOn::MasterToFinalize => "gate FINISHED — run `clank finish`".into(),
        WaitingOn::MasterToCommit => "gate approved but plan file dirty".into(),
    }
}

fn active_summary(fold: &clank_core::repo_state::RepoState, basename: &str) -> String {
    let names: Vec<String> = fold
        .plans
        .keys()
        .map(|k| format!("{basename}/{}.md", k.as_str()))
        .collect();
    if names.is_empty() {
        "(none)".into()
    } else {
        names.join(", ")
    }
}

pub(crate) fn short_sha(s: &str) -> &str {
    &s[..s.len().min(7)]
}

fn head_info(repo: &Path) -> (Option<String>, Option<String>, Option<String>) {
    let branch = git_output(repo, &["symbolic-ref", "--short", "HEAD"]);
    let sha = git_output(repo, &["rev-parse", "HEAD"]);
    let subject = git_output(repo, &["log", "-1", "--format=%s"]);
    (branch, sha, subject)
}

fn worktree_dirty(repo: &Path) -> anyhow::Result<bool> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()?;
    Ok(!output.stdout.is_empty())
}

fn git_output(repo: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
