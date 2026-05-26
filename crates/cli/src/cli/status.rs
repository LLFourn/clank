//! `clank status` — read-only summary of the current repo's Clank
//! state. Folds the repo, projects per-plan via `clank-core::plan_view`,
//! then renders. Never blocks.

use std::io::Write as _;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use super::{StatusArgs, repo_basename, resolve_repo};
use crate::cli::plan_resolve::parse_arg;
use crate::fs_review_lookup::FsReviewLookup;
use crate::lifecycle::PlanKey;
use crate::repo_state::RepoState;
use clank_core::plan_view::WaitingOn;
use clank_core::wait::PlanWorkState;

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

    let state = crate::rebuild::rebuild_repo_with_policy(&repo, policy)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;

    let (branch, head_sha, head_subject) = head_info(&repo);
    let worktree_dirty = worktree_dirty(&repo)?;

    let config = crate::cli::config::load(&repo);
    let work_policy = clank_core::wait::WorkPolicy {
        plan_feedback: config.review.plan_feedback,
        adhoc_feedback: config.review.adhoc_feedback,
    };
    let reviews = FsReviewLookup::new(&repo, state.head.as_ref());
    let work_status = state.fold.derive_status(&reviews, &work_policy);

    let selected = select_plans(&state, &basename, args.plan.as_deref(), false)?;
    let views: Vec<&PlanWorkState> = work_status
        .plans
        .iter()
        .filter(|ps| selected.contains(&ps.plan))
        .collect();

    let blocks = crate::cli::block::scan_blocks(&repo);
    let queue_count = crate::cli::queue::scan_queue(&repo).len();

    if args.json {
        let json = build_json(
            &basename,
            branch.clone(),
            head_sha.clone(),
            head_subject.clone(),
            worktree_dirty,
            &views,
            &state,
            &blocks,
            queue_count,
        );
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        print_human(
            &repo,
            branch.as_deref(),
            head_sha.as_deref(),
            worktree_dirty,
            &views,
            &state,
            &blocks,
            queue_count,
        );
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
        let state = crate::rebuild::rebuild_repo_with_policy(&repo, policy)
            .await
            .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;

        let (branch, head_sha, head_subject) = head_info(&repo);
        let worktree_dirty = worktree_dirty(&repo)?;

        let config = crate::cli::config::load(&repo);
        let work_policy = clank_core::wait::WorkPolicy {
            plan_feedback: config.review.plan_feedback,
            adhoc_feedback: config.review.adhoc_feedback,
        };
        let reviews = FsReviewLookup::new(&repo, state.head.as_ref());
        let work_status = state.fold.derive_status(&reviews, &work_policy);

        let selected = select_plans(&state, &basename, plan_arg, true)?;
        let views: Vec<&PlanWorkState> = work_status
            .plans
            .iter()
            .filter(|ps| selected.contains(&ps.plan))
            .collect();

        let blocks = crate::cli::block::scan_blocks(&repo);
        let queue_count = crate::cli::queue::scan_queue(&repo).len();

        let output = if json {
            let val = build_json(
                &basename,
                branch.clone(),
                head_sha.clone(),
                head_subject.clone(),
                worktree_dirty,
                &views,
                &state,
                &blocks,
                queue_count,
            );
            serde_json::to_string(&val)?
        } else {
            render_human_to_string(
                &repo,
                branch.as_deref(),
                head_sha.as_deref(),
                worktree_dirty,
                &views,
                &state,
                &blocks,
                queue_count,
            )
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

/// Pick the plans to render based on the CLI flags. In watch mode,
/// a `--plan` that is not active is checked against finished plans
/// instead of erroring, so callers can observe the finished state.
fn select_plans(
    state: &RepoState,
    basename: &str,
    plan_arg: Option<&str>,
    watch_mode: bool,
) -> anyhow::Result<Vec<PlanKey>> {
    if let Some(raw) = plan_arg {
        let stem = parse_arg(raw, basename)?;
        let key = PlanKey::parse(&stem)
            .map_err(|e| anyhow::anyhow!("invalid plan stem `{stem}`: {e}"))?;
        if state.fold.plans.contains_key(&key) {
            return Ok(vec![key]);
        }
        if watch_mode {
            let finished = state
                .fold
                .finished_plans
                .iter()
                .any(|fp| fp.plan == key);
            if finished {
                return Ok(vec![]);
            }
        }
        anyhow::bail!(
            "plan `{basename}/{stem}.md` not active. active: {}",
            active_summary(state, basename)
        );
    }
    Ok(state.fold.plans.keys().cloned().collect())
}


fn build_json(
    basename: &str,
    branch: Option<String>,
    head_sha: Option<String>,
    head_subject: Option<String>,
    worktree_dirty: bool,
    views: &[&PlanWorkState],
    state: &RepoState,
    blocks: &[crate::cli::block::BlockEntry],
    queue_count: usize,
) -> serde_json::Value {
    let plans: Vec<serde_json::Value> = views
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
    let finished: Vec<serde_json::Value> = if views.is_empty() {
        state
            .fold
            .finished_plans
            .last()
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
    let all_blocks: Vec<serde_json::Value> = blocks
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
        "repo_basename": basename,
        "branch": branch,
        "head_sha": head_sha,
        "head_subject": head_subject,
        "worktree_dirty": worktree_dirty,
        "plans": plans,
        "finished_plans": finished,
        "blocks": all_blocks,
        "queue_count": queue_count,
    })
}

fn render_human_to_string(
    repo: &Path,
    branch: Option<&str>,
    head_sha: Option<&str>,
    worktree_dirty: bool,
    views: &[&PlanWorkState],
    state: &RepoState,
    blocks: &[crate::cli::block::BlockEntry],
    queue_count: usize,
) -> String {
    let mut out = String::new();
    use std::fmt::Write as _;

    let _ = writeln!(out, "repo:   {}", repo.display());
    if let Some(b) = branch {
        let _ = writeln!(out, "branch: {b}");
    }
    if let Some(s) = head_sha {
        let _ = writeln!(out, "head:   {s}");
    }
    let _ = writeln!(out, "dirty:  {}", if worktree_dirty { "yes" } else { "no" });
    if queue_count > 0 {
        let _ = writeln!(out, "queue:  {queue_count} item{}", if queue_count == 1 { "" } else { "s" });
    }

    if views.is_empty() && state.fold.plans.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "no active plan, nothing pending");
    }
    for v in views {
        let _ = writeln!(out);
        let _ = writeln!(out, "plan: {}", v.plan.as_str());
        let _ = writeln!(out, "  latest reviewable: {}", short_sha(v.sha.as_str()));
        let _ = writeln!(out, "  gate:              {}", v.gate);
        let _ = writeln!(out, "  waiting on:        {}", waiting_actor(&v.waiting_on));
        let _ = writeln!(out, "  reason:            {}", waiting_reason(&v.waiting_on));
    }

    if views.is_empty() {
        if let Some(fp) = state.fold.finished_plans.last() {
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "last finished: {} (finalized {})",
                fp.plan.as_str(),
                short_sha(fp.finalized_at.as_str())
            );
        }
    }

    if !blocks.is_empty() {
        let has_any = blocks.iter().any(|b| b.answer.is_none() || b.answer.is_some());
        if has_any {
            let _ = writeln!(out);
            let _ = writeln!(out, "blocks:");
            for b in blocks {
                let scope = b.plan.as_deref().unwrap_or("repo");
                if let Some(ref answer) = b.answer {
                    let _ = writeln!(out, "  UNBLOCKED ({}, scope: {}): {}", b.agent, scope, b.question);
                    let _ = writeln!(out, "    answer: {answer}");
                } else {
                    let _ = writeln!(out, "  BLOCKED ({}, scope: {}): {}", b.agent, scope, b.question);
                }
            }
        }
    }

    out.trim_end_matches('\n').to_string()
}

fn print_human(
    repo: &Path,
    branch: Option<&str>,
    head_sha: Option<&str>,
    worktree_dirty: bool,
    views: &[&PlanWorkState],
    state: &RepoState,
    blocks: &[crate::cli::block::BlockEntry],
    queue_count: usize,
) {
    println!(
        "{}",
        render_human_to_string(repo, branch, head_sha, worktree_dirty, views, state, blocks, queue_count)
    );
}

/// Short actor label: WHO holds the next move.
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

/// Explanation: WHY the actor is on the hook.
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

fn active_summary(state: &RepoState, basename: &str) -> String {
    let names: Vec<String> = state
        .fold
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
