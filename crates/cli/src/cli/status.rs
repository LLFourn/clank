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
use clank_core::wait::PlanWorkState;

struct StatusSnapshot {
    repo_path: PathBuf,
    basename: String,
    branch: Option<String>,
    head_sha: Option<String>,
    head_subject: Option<String>,
    worktree_dirty: bool,
    plans: Vec<PlanWorkState>,
    last_finished: Option<FinishedPlan>,
    blocks: Vec<crate::cli::block::BlockEntry>,
    queue_count: usize,
}

impl StatusSnapshot {
    async fn build_async(
        repo: &Path,
        basename: &str,
        policy: crate::rebuild::CachePolicy,
        plan_arg: Option<&str>,
    ) -> anyhow::Result<Self> {
        let state = crate::rebuild::rebuild_repo_with_policy(repo, policy)
            .await
            .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
        Self::from_state(repo, basename, &state, plan_arg)
    }

    fn from_state(
        repo: &Path,
        basename: &str,
        state: &RepoState,
        plan_arg: Option<&str>,
    ) -> anyhow::Result<Self> {
        let (branch, head_sha, head_subject) = head_info(repo);
        let worktree_dirty = worktree_dirty(repo)?;

        let config = crate::cli::config::load(repo);
        let work_policy = clank_core::wait::WorkPolicy {
            plan_feedback: config.review.plan_feedback,
            adhoc_feedback: config.review.adhoc_feedback,
        };
        let reviews = crate::fs_review_lookup::FsReviewLookup::new(repo, state.head.as_ref());
        let work_status = state.fold.derive_status(&reviews, &work_policy);

        let (plans, last_finished) =
            select_plans_and_finished(&work_status.plans, &state.fold, basename, plan_arg)?;

        let blocks = crate::cli::block::scan_blocks(repo);
        let queue_count = crate::cli::queue::scan_queue(repo).len();

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
            queue_count,
        })
    }

    fn to_json(&self) -> serde_json::Value {
        let plans: Vec<serde_json::Value> = self
            .plans
            .iter()
            .map(|v| {
                serde_json::json!({
                    "plan": v.plan.as_str(),
                    "latest_reviewable_sha": v.sha.as_str(),
                    "gate_state": v.gate,
                    "waiting_on": format!("{:?}", v.waiting_on),
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

        serde_json::json!({
            "repo_basename": self.basename,
            "branch": self.branch,
            "head_sha": self.head_sha,
            "head_subject": self.head_subject,
            "worktree_dirty": self.worktree_dirty,
            "plans": plans,
            "finished_plans": finished,
            "blocks": all_blocks,
            "queue_count": self.queue_count,
        })
    }

    fn to_human(&self) -> String {
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
        if self.queue_count > 0 {
            let _ = writeln!(
                out,
                "queue:  {} item{}",
                self.queue_count,
                if self.queue_count == 1 { "" } else { "s" }
            );
        }

        if self.plans.is_empty() && self.last_finished.is_none() {
            let _ = writeln!(out);
            let _ = writeln!(out, "no active plan, nothing pending");
        }

        for v in &self.plans {
            let _ = writeln!(out);
            let _ = writeln!(out, "plan: {}", v.plan.as_str());
            let _ = writeln!(out, "  latest reviewable: {}", short_sha(v.sha.as_str()));
            let _ = writeln!(out, "  gate:              {}", v.gate);
            let _ = writeln!(out, "  waiting on:        {}", waiting_actor(&v.waiting_on));
            let _ = writeln!(out, "  reason:            {}", waiting_reason(&v.waiting_on));
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

    if args.watch {
        return run_watch(repo, basename, policy, args.json, args.plan.as_deref()).await;
    }

    let snapshot =
        StatusSnapshot::build_async(&repo, &basename, policy, args.plan.as_deref()).await?;

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
            StatusSnapshot::build_async(&repo, &basename, policy, plan_arg).await?;

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
            Ok(()) => {
                while rx.recv_timeout(Duration::from_millis(200)).is_ok() {}
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("filesystem watcher disconnected")
            }
        }
    }
}

fn build_watcher(tx: mpsc::Sender<()>) -> anyhow::Result<RecommendedWatcher> {
    Ok(notify::recommended_watcher(
        move |res: notify::Result<notify::Event>| {
            if res.is_ok() {
                let _ = tx.send(());
            }
        },
    )?)
}

fn attach_watcher(watcher: &mut RecommendedWatcher, repo: &Path) -> anyhow::Result<()> {
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

        let last_finished = fold
            .finished_plans
            .iter()
            .rev()
            .find(|fp| fp.plan == key)
            .cloned();

        if last_finished.is_some() {
            return Ok((vec![], last_finished));
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

fn waiting_actor(w: &WaitingOn) -> String {
    match w {
        WaitingOn::FirstReview => "any reviewer".into(),
        WaitingOn::ReviewerApprovalsMissing { missing } => missing
            .iter()
            .map(|a| a.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        WaitingOn::MasterToRevise { .. }
        | WaitingOn::MasterToImplement
        | WaitingOn::MasterToFinalize
        | WaitingOn::MasterToCommit => "master".into(),
    }
}

fn waiting_reason(w: &WaitingOn) -> String {
    match w {
        WaitingOn::FirstReview => "no reviewer has weighed in yet".into(),
        WaitingOn::ReviewerApprovalsMissing { missing } => {
            let names = missing
                .iter()
                .map(|a| a.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("missing approval from {names}")
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
        WaitingOn::MasterToImplement => {
            "gate approved — start implementation under [<stem>]".into()
        }
        WaitingOn::MasterToFinalize => "gate approved — run `clank finish`".into(),
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

fn short_sha(s: &str) -> &str {
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
