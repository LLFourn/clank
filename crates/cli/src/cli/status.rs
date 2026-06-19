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
    /// `Some` iff the worktree is dirty — carries the +/− line
    /// counts vs HEAD and the untracked-file count
    /// (tui-event-driven-dirty-stats).
    pub(crate) dirty: Option<DirtyStats>,
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
    /// headers + commits + reviews), NEWEST FIRST (the git-log
    /// convention every log display follows; lloyd) — the TUI's
    /// log pane takes the head and styles per row kind. Built from the fold's LogEvents (subjects carried; no
    /// per-event git shelling). NOT emitted by `to_json` — the
    /// status --json log shape is unchanged; `clank log --json`
    /// is the machine log surface (log-plan-umbrellas, ruthless
    /// 3ea8580 concern 1). Plans: status-tui-live-log,
    /// log-plan-umbrellas.
    pub(crate) log_rows: Vec<crate::cli::log::OnelineRow>,
    /// Active GitHub PR reviews (clank-pr-review-mode), for the
    /// `pr` gauge. Empty in the common (no-PR-review) case.
    pub(crate) pr_reviews: Vec<clank_core::wait::PrReviewWorkState>,
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
        let dirty = dirty_stats(repo)?;

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
            dirty,
            plans,
            last_finished,
            blocks,
            queue,
            master,
            shelved,
            log_rows,
            pr_reviews: work_status.pr_reviews,
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
            "worktree_dirty": self.dirty.is_some(),
            "plans": plans,
            "finished_plans": finished,
            "blocks": all_blocks,
        });
        if let Some(d) = &self.dirty {
            // Additive (tui-event-driven-dirty-stats);
            // worktree_dirty kept for existing consumers.
            obj["dirty_stats"] = serde_json::json!({
                "insertions": d.insertions,
                "deletions": d.deletions,
                "untracked": d.untracked,
            });
        }
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
            match &self.dirty {
                Some(d) => format!("yes ({})", dirty_summary(d)),
                None => "no".to_string(),
            }
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
        for pr in &self.pr_reviews {
            let url = pr_url(&pr.repo, pr.pr);
            if pr.round == 0 {
                // Not opened yet: master is drafting, no reviewers.
                let _ = writeln!(out, "pr #{}: round 0 — master drafting", pr.pr);
            } else {
                let waiting = if pr.missing_reviewers.is_empty() {
                    "master".to_string()
                } else {
                    pr.missing_reviewers
                        .iter()
                        .map(|l| l.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let _ = writeln!(
                    out,
                    "pr #{}: round {} ({}) — waiting on {waiting}",
                    pr.pr,
                    pr.round,
                    pr.gate.as_str(),
                );
            }
            let _ = writeln!(out, "  {url}");
        }

        if self.plans.is_empty() && self.last_finished.is_none() && self.pr_reviews.is_empty() {
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
    let _watcher = watch_status_paths(tx, &repo)?;

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

        // Event-driven: worktree edits, .clank writes, and git-dir
        // changes all arrive as watcher events now, so no fast
        // poll. The long timeout is a backstop against watcher
        // pathologies the error channel doesn't surface.
        match rx.recv_timeout(Duration::from_secs(60)) {
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
/// TUI styles them). Scope: repo-wide recent activity, ALWAYS — the
/// pane shows the same window regardless of how many plans are active
/// (status-tui-unified-log; the old single-active-plan narrowing made
/// a just-finished plan's commits vanish the moment one plan was
/// active). Best-effort: any failure yields an empty pane, never a
/// status error.
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

    let reviewable: Vec<crate::lifecycle::CommitSha> = events
        .iter()
        .filter_map(|e| match e {
            LogEvent::PlanCommit { sha, .. }
            | LogEvent::PlanIntro { sha, .. }
            | LogEvent::AdHoc { sha, .. } => Some(sha.clone()),
            _ => None,
        })
        .collect();
    let reviews = crate::cli::log::collect_reviews(repo, &reviewable);
    // Newest first, like `clank log` — the pane reads top-down.
    let newest_first: Vec<&LogEvent> = events.iter().rev().collect();
    crate::cli::log::oneline_rows(&newest_first, &reviews)
}

/// Decides which filesystem events wake the status loops. This is an
/// ALLOWLIST, not "anything under `.clank`" (status-tui-watch-cpu):
/// wakes on
/// - the workflow-state signal dirs under `.clank` ([`CLANK_WAKE_DIRS`]:
///   plans/queue/blocks/agents/finished/config) — feedback/queue/agent
///   state are load-bearing even though gitignored;
/// - anything under the git dir (HEAD moves, ref updates);
/// - any `.gitignore` change (which also refreshes the matcher);
/// - any worktree path the gitignore rules do NOT match (keeps
///   `dirty:` fresh on tracked edits).
///
/// It deliberately does NOT wake on the rest of `.clank`: the derived
/// fold `cache`, generated `html`, the `zellij` layout, and — the big
/// one — nested worktrees under `.clank/worktrees/<name>/`, which are
/// whole separate repos whose builds + caches would otherwise wake
/// this pane. Gitignore-matched worktree paths (`target/`, `*.log`)
/// are dropped too — a `cargo build`'s thousands of `target/` files
/// say nothing about clank state or worktree dirt.
///
/// The matcher anchors at the repo root's `.gitignore` (plus
/// `.git/info/exclude`); nested `.gitignore` files aren't modeled —
/// a path only they ignore costs a harmless debounced wake.
/// TRACKED-but-gitignored files (`git add -f`) are dropped like
/// any ignored path: their edits refresh `dirty:` on the 60s
/// backstop, not instantly.
pub(crate) struct WakeFilter {
    repo_root: PathBuf,
    git_dir: PathBuf,
    clank_root: PathBuf,
    matcher: ignore::gitignore::Gitignore,
}

/// The first path component under `.clank/` that carries a real
/// workflow signal. Everything else under `.clank` is derived or
/// foreign and must NOT wake the loop (status-tui-watch-cpu): the
/// fold `cache`, generated `html`, the `zellij` layout, queue
/// `stubs` — and, critically, nested worktrees under
/// `.clank/worktrees/<name>/`, which are whole separate repos with
/// their own `src/`, `target/`, `.git`, and `.clank/cache`.
const CLANK_WAKE_DIRS: &[&str] = &[
    "plans",
    "queue",
    "blocks",
    "agents", // agents/<label>/feedback — the gate signal
    "finished",
    "config.json",
];

impl WakeFilter {
    pub(crate) fn new(repo_root: &Path, git_dir: &Path) -> Self {
        // FSEvents delivers canonical paths (`/private/var/…`);
        // compare against canonical roots or every prefix check
        // misses on symlinked locations (e.g. macOS tempdirs).
        let repo_root = dunce::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
        let git_dir = dunce::canonicalize(git_dir).unwrap_or_else(|_| git_dir.to_path_buf());
        Self {
            clank_root: repo_root.join(".clank"),
            matcher: Self::build_matcher(&repo_root, &git_dir),
            repo_root,
            git_dir,
        }
    }

    fn build_matcher(repo_root: &Path, git_dir: &Path) -> ignore::gitignore::Gitignore {
        let mut builder = ignore::gitignore::GitignoreBuilder::new(repo_root);
        let _ = builder.add(repo_root.join(".gitignore"));
        let _ = builder.add(git_dir.join("info").join("exclude"));
        builder
            .build()
            .unwrap_or_else(|_| ignore::gitignore::Gitignore::empty())
    }

    /// True → the event at `path` should wake the loop.
    pub(crate) fn wakes(&mut self, path: &Path) -> bool {
        // Commits move refs and the fold reads them. (Object churn
        // during a commit is bursty, not a sustained storm.)
        if path.starts_with(&self.git_dir) {
            return true;
        }
        // Under `.clank`, wake ONLY on the workflow-state signal dirs
        // (`CLANK_WAKE_DIRS`). This is an ALLOWLIST, not "anything under
        // `.clank`": the repo is watched recursively, and `.clank`
        // holds the derived `cache`/`html`, the `zellij` layout, and
        // nested worktrees under `.clank/worktrees/<name>/` whose own
        // builds + caches would otherwise wake this pane on every
        // `cargo build` across a dozen worktrees (status-tui-watch-cpu).
        if let Ok(rel) = path.strip_prefix(&self.clank_root) {
            let head = rel.components().next().and_then(|c| c.as_os_str().to_str());
            return head.is_some_and(|h| CLANK_WAKE_DIRS.contains(&h));
        }
        if path.file_name().is_some_and(|n| n == ".gitignore") {
            self.matcher = Self::build_matcher(&self.repo_root, &self.git_dir);
            return true;
        }
        // Paths outside the root (the matcher would panic on them)
        // shouldn't arrive; if one does, wake conservatively.
        if !path.starts_with(&self.repo_root) {
            return true;
        }
        // Working-tree path: wake unless gitignored — keeps `dirty:`
        // fresh on tracked edits while dropping build artifacts
        // (`target/`, `*.log`, …). `is_dir` races with deletion; a
        // vanished path reads as non-dir, only loosening toward a wake.
        !self
            .matcher
            .matched_path_or_any_parents(path, path.is_dir())
            .is_ignore()
    }

    #[cfg(test)]
    fn with_rules(repo_root: &Path, git_dir: &Path, rules: &[&str]) -> Self {
        let mut builder = ignore::gitignore::GitignoreBuilder::new(repo_root);
        for rule in rules {
            builder.add_line(None, rule).expect("test rule");
        }
        Self {
            repo_root: repo_root.to_path_buf(),
            git_dir: git_dir.to_path_buf(),
            clank_root: repo_root.join(".clank"),
            matcher: builder.build().expect("test matcher"),
        }
    }
}

/// Watcher for the status loops: the repo root recursively (working
/// tree, `.clank`, and `.git` when embedded) plus the git dir when
/// it lives elsewhere (linked worktrees) — events filtered through
/// [`WakeFilter`]. Watcher ERRORS also wake: notify signals queue
/// overflow as an error event, and the right response is one cheap
/// rebuild, not silent staleness.
pub(crate) fn watch_status_paths(
    tx: mpsc::Sender<()>,
    repo: &Path,
) -> anyhow::Result<RecommendedWatcher> {
    let clank_root = repo.join(".clank");
    if let Err(e) = std::fs::create_dir_all(&clank_root) {
        anyhow::bail!("ensure `{}` exists: {e}", clank_root.display());
    }
    let git_dir = git_resolve_dir(repo)?;
    let mut filter = WakeFilter::new(repo, &git_dir);
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(event) => {
                // Path-less events (rescan notices) wake conservatively.
                if event.paths.is_empty() || event.paths.iter().any(|p| filter.wakes(p)) {
                    let _ = tx.send(());
                }
            }
            Err(_) => {
                let _ = tx.send(());
            }
        }
    })?;
    watcher
        .watch(repo, RecursiveMode::Recursive)
        .map_err(|e| anyhow::anyhow!("watch `{}` failed: {e}", repo.display()))?;
    if !git_dir.starts_with(repo) {
        watcher
            .watch(&git_dir, RecursiveMode::Recursive)
            .map_err(|e| anyhow::anyhow!("watch `{}` failed: {e}", git_dir.display()))?;
    }
    Ok(watcher)
}

/// Forward SIGWINCH into the wake channel so a terminal resize is
/// just another event. A forwarding thread, not a signal handler:
/// `mpsc::Sender::send` is not async-signal-safe. The thread
/// blocks in `forever()` and only notices a dropped receiver on
/// the next signal — it leaks until process exit, which is fine
/// for the once-per-process TUI.
pub(crate) fn spawn_sigwinch_forwarder(tx: mpsc::Sender<()>) -> anyhow::Result<()> {
    let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])?;
    std::thread::spawn(move || {
        for _ in signals.forever() {
            if tx.send(()).is_err() {
                break;
            }
        }
    });
    Ok(())
}

fn git_resolve_dir(repo: &Path) -> anyhow::Result<std::path::PathBuf> {
    let dir = crate::git_io::git_dir(repo)?;
    Ok(dunce::canonicalize(&dir).unwrap_or(dir))
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
    let branch = crate::git_io::current_branch(repo).ok().flatten();
    let head = crate::git_io::rev_parse_head(repo).ok().flatten();
    let sha = head.as_ref().map(|s| s.as_str().to_string());
    let subject = head
        .as_ref()
        .and_then(|s| crate::git_io::commit_subject(repo, s).ok());
    (branch, sha, subject)
}

/// Worktree dirt summary: +/− line counts vs HEAD (staged and
/// unstaged together) and the untracked-file count. Untracked
/// lines are NOT folded into the +/− numbers — a diff against
/// HEAD doesn't see them, and pretending otherwise lies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DirtyStats {
    pub(crate) insertions: u64,
    pub(crate) deletions: u64,
    pub(crate) untracked: u64,
}

/// `None` = clean. Both probes run `--no-optional-locks`: they
/// execute on every watcher-driven repaint, and a plain
/// `git status` opportunistically rewrites `.git/index` (stat
/// refresh) — an event the watcher would see, waking the loop the
/// probe itself was serving (self-wake feedback).
pub(crate) fn dirty_stats(repo: &Path) -> anyhow::Result<Option<DirtyStats>> {
    let porcelain = git_nol(repo, &["status", "--porcelain"])?;
    if porcelain.is_empty() {
        return Ok(None);
    }
    // `diff HEAD` fails on an unborn HEAD — degrade to 0/0 (the
    // untracked count still tells the story there).
    let shortstat = git_nol(repo, &["diff", "HEAD", "--shortstat"]).unwrap_or_default();
    let (insertions, deletions) = parse_shortstat(&shortstat);
    Ok(Some(DirtyStats {
        insertions,
        deletions,
        untracked: count_untracked(&porcelain),
    }))
}

/// Run git with `--no-optional-locks`, returning stdout. Errors on
/// spawn failure or non-zero exit.
fn git_nol(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = std::process::Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The PR's GitHub URL from its `owner/name` slug + number. The one
/// place clank formats a github URL, shared by the text and TUI
/// status surfaces (the TUI makes it a clickable OSC 8 hyperlink).
pub(crate) fn pr_url(repo: &str, pr: u32) -> String {
    format!("https://github.com/{repo}/pull/{pr}")
}

/// `+12 −3 · 2 untracked`, omitting zero parts. A dirty tree whose
/// numbers are all zero (e.g. a mode-only change) reads `changes`.
pub(crate) fn dirty_summary(d: &DirtyStats) -> String {
    let mut parts = Vec::new();
    if d.insertions > 0 || d.deletions > 0 {
        parts.push(format!("+{} −{}", d.insertions, d.deletions));
    }
    if d.untracked > 0 {
        parts.push(format!("{} untracked", d.untracked));
    }
    if parts.is_empty() {
        return "changes".to_string();
    }
    parts.join(" · ")
}

fn count_untracked(porcelain: &str) -> u64 {
    porcelain.lines().filter(|l| l.starts_with("??")).count() as u64
}

/// Parse `git diff --shortstat` output, e.g.
/// ` 3 files changed, 12 insertions(+), 3 deletions(-)` → (12, 3).
/// Either clause may be absent; empty input → (0, 0).
fn parse_shortstat(s: &str) -> (u64, u64) {
    let (mut insertions, mut deletions) = (0, 0);
    for part in s.trim().split(',') {
        let Some((num, rest)) = part.trim().split_once(' ') else {
            continue;
        };
        let Ok(n) = num.parse::<u64>() else {
            continue;
        };
        if rest.starts_with("insertion") {
            insertions = n;
        } else if rest.starts_with("deletion") {
            deletions = n;
        }
    }
    (insertions, deletions)
}

#[cfg(test)]
mod dirty_and_wake_tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn parse_shortstat_matrix() {
        let cases = [
            ("", (0, 0)),
            (
                " 3 files changed, 12 insertions(+), 3 deletions(-)\n",
                (12, 3),
            ),
            (" 1 file changed, 1 insertion(+)\n", (1, 0)),
            (" 1 file changed, 5 deletions(-)\n", (0, 5)),
            (" 2 files changed\n", (0, 0)),
        ];
        for (input, want) in cases {
            assert_eq!(parse_shortstat(input), want, "input: {input:?}");
        }
    }

    #[test]
    fn count_untracked_reads_porcelain() {
        assert_eq!(
            count_untracked(" M src/a.rs\n?? new.rs\n?? dir/\nA  staged.rs\n"),
            2
        );
        assert_eq!(count_untracked(""), 0);
    }

    #[test]
    fn dirty_summary_omits_zero_parts() {
        let s = |i, d, u| {
            dirty_summary(&DirtyStats {
                insertions: i,
                deletions: d,
                untracked: u,
            })
        };
        assert_eq!(s(12, 3, 0), "+12 −3");
        assert_eq!(s(12, 3, 2), "+12 −3 · 2 untracked");
        assert_eq!(s(0, 0, 2), "2 untracked");
        assert_eq!(s(0, 0, 0), "changes");
        assert_eq!(s(0, 5, 0), "+0 −5");
    }

    #[test]
    fn wake_filter_matrix() {
        let root = Path::new("/repo");
        let git_dir = Path::new("/repo/.git");
        let mut f = WakeFilter::with_rules(root, git_dir, &["/target/", "*.log"]);

        // .clank workflow-state SIGNAL dirs + .git wake.
        assert!(f.wakes(Path::new("/repo/.clank/agents/codex/feedback/abc.md")));
        assert!(f.wakes(Path::new("/repo/.clank/blocks/q.md")));
        assert!(f.wakes(Path::new("/repo/.clank/plans/foo.md")));
        assert!(f.wakes(Path::new("/repo/.clank/queue/500-foo.md")));
        assert!(f.wakes(Path::new("/repo/.clank/finished/foo.md")));
        assert!(f.wakes(Path::new("/repo/.clank/config.json")));
        assert!(f.wakes(Path::new("/repo/.git/HEAD")));

        // DERIVED / foreign state under `.clank` must NOT wake — this
        // is the allowlist, not "anything under .clank"
        // (status-tui-watch-cpu). The fold cache self-triggers a
        // render→write-cache→wake loop; html/zellij are generated.
        assert!(!f.wakes(Path::new("/repo/.clank/cache/repo-state/abc123.7.v10.bin")));
        assert!(!f.wakes(Path::new("/repo/.clank/html/index.html")));
        assert!(!f.wakes(Path::new("/repo/.clank/zellij/layout.kdl")));

        // NESTED WORKTREES under `.clank/worktrees/<name>/` are whole
        // separate repos. Their builds AND their own caches must NOT
        // wake this pane — the regression ruthless caught: the old
        // `starts_with(.clank)` short-circuit woke on all of these.
        assert!(!f.wakes(Path::new("/repo/.clank/worktrees/wt1/target/debug/junk.o")));
        assert!(!f.wakes(Path::new(
            "/repo/.clank/worktrees/wt1/.clank/cache/repo-state/x.v10.bin"
        )));
        assert!(!f.wakes(Path::new("/repo/.clank/worktrees/wt1/src/lib.rs")));
        // ...even a worktree's OWN plan file: it's that worktree's
        // tui's job, not ours.
        assert!(!f.wakes(Path::new("/repo/.clank/worktrees/wt1/.clank/plans/bar.md")));

        // Top-level working tree: gitignored → drop, tracked → wake
        // (keeps `dirty:` fresh on source saves).
        assert!(!f.wakes(Path::new("/repo/target/debug/build/junk.o")));
        assert!(!f.wakes(Path::new("/repo/build.log")));
        assert!(f.wakes(Path::new("/repo/src/lib.rs")));
        assert!(f.wakes(Path::new("/repo/Cargo.toml")));
        // .gitignore changes always wake (and refresh the matcher).
        assert!(f.wakes(Path::new("/repo/.gitignore")));
    }

    fn git(repo: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn fixture_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        git(r, &["init", "--quiet", "-b", "main"]);
        git(r, &["config", "user.email", "t@t"]);
        git(r, &["config", "user.name", "t"]);
        std::fs::write(r.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        git(r, &["add", "-A"]);
        git(r, &["commit", "--quiet", "-m", "base"]);
        dir
    }

    #[tokio::test]
    async fn status_shows_adhoc_commits_with_one_active_plan() {
        // show-adhoc-commits-in-status (reproduce-first): with exactly
        // ONE active plan, recent_log_rows auto-applied a single-plan
        // filter that DROPPED ad-hoc events. An ad-hoc commit (no
        // `[plan]` tag) must still appear in the status log.
        let dir = fixture_repo();
        let r = dir.path();
        // Plan intro → adopts the repo AND makes `foo` the lone active
        // plan (so the single-plan filter engages).
        std::fs::create_dir_all(r.join(".clank/plans")).unwrap();
        std::fs::write(r.join(".clank/plans/foo.md"), "# foo\n").unwrap();
        git(r, &["add", "-A"]);
        git(r, &["commit", "--quiet", "-m", "[foo] intro"]);
        // Post-adoption ad-hoc commit. Tagged `[bar]` — a NON-plan tag
        // (no `.clank/plans/bar.md`), which is what makes it ad-hoc
        // rather than inherited into `foo` via the active-plan hint
        // (this is exactly the real `[animations]` case). `foo` stays
        // the lone active plan, so the single-plan filter engages.
        std::fs::write(r.join("scratch.txt"), "scratch\n").unwrap();
        git(r, &["add", "-A"]);
        git(r, &["commit", "--quiet", "-m", "[bar] sketch the thing"]);

        let snap = StatusSnapshot::build_async(
            r,
            "repo",
            None,
            crate::rebuild::CachePolicy::Bypass,
            None,
            false,
        )
        .await
        .unwrap();

        let subjects: Vec<&str> = snap
            .log_rows
            .iter()
            .filter_map(|row| match row {
                crate::cli::log::OnelineRow::Commit { subject, .. } => Some(subject.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            subjects.iter().any(|s| s.contains("sketch the thing")),
            "ad-hoc commit must appear in status log even with one active plan; got subjects: {subjects:?}"
        );
    }

    #[test]
    fn dirty_stats_reports_lines_and_untracked() {
        let dir = fixture_repo();
        let r = dir.path();
        assert_eq!(dirty_stats(r).unwrap(), None, "clean tree");

        // 1 line modified (one in, one out), 2 added; one untracked.
        std::fs::write(r.join("a.txt"), "one\nTWO\nthree\nfour\nfive\n").unwrap();
        std::fs::write(r.join("new.txt"), "x\n").unwrap();
        let d = dirty_stats(r).unwrap().expect("dirty");
        assert_eq!((d.insertions, d.deletions), (3, 1));
        assert_eq!(d.untracked, 1);
    }

    #[test]
    fn dirty_probe_never_writes_the_index() {
        // The self-wake hazard: a probe that refreshes .git/index
        // would fire the watcher that triggered the probe. With
        // --no-optional-locks the index bytes must stay untouched
        // even when stat info is stale.
        let dir = fixture_repo();
        let r = dir.path();
        std::fs::write(r.join("a.txt"), "one\ntwo\nthree\nmore\n").unwrap();
        // Make stat info stale so a plain `git status` would want
        // to refresh the index.
        let index = r.join(".git/index");
        let before = std::fs::read(&index).unwrap();
        let mtime_before = std::fs::metadata(&index).unwrap().modified().unwrap();
        let _ = dirty_stats(r).unwrap();
        assert_eq!(
            std::fs::read(&index).unwrap(),
            before,
            "index bytes changed"
        );
        assert_eq!(
            std::fs::metadata(&index).unwrap().modified().unwrap(),
            mtime_before,
            "index mtime changed"
        );
    }

    #[test]
    fn watcher_wakes_on_tracked_edit_not_on_ignored() {
        let dir = fixture_repo();
        let r = dir.path();
        std::fs::write(r.join(".gitignore"), "/target/\n").unwrap();
        git(r, &["add", "-A"]);
        git(r, &["commit", "--quiet", "-m", "gitignore"]);
        std::fs::create_dir_all(r.join("target/debug")).unwrap();

        let (tx, rx) = mpsc::channel::<()>();
        let _watcher = watch_status_paths(tx, r).unwrap();
        // Let the watcher settle (registration races the first writes).
        std::thread::sleep(Duration::from_millis(250));
        while rx.try_recv().is_ok() {}

        // Ignored path: no wake.
        std::fs::write(r.join("target/debug/out.o"), "junk").unwrap();
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "ignored build artifact woke the loop"
        );

        // Tracked-file edit: wakes (this is what keeps `dirty:`
        // fresh without any poll).
        std::fs::write(r.join("a.txt"), "edited\n").unwrap();
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "tracked edit did not wake the loop"
        );
    }
}
