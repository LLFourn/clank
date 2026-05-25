//! `clank status` — read-only summary of the current repo's Clank
//! state. Folds the repo, projects per-plan via `clank-core::plan_view`,
//! then renders. Never blocks.

use std::path::Path;

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

    let selected = select_plans(&state, &basename, args.plan.as_deref())?;
    let views: Vec<&PlanWorkState> = work_status
        .plans
        .iter()
        .filter(|ps| selected.contains(&ps.plan))
        .collect();

    if args.json {
        let json = build_json(
            &basename,
            branch.clone(),
            head_sha.clone(),
            head_subject.clone(),
            worktree_dirty,
            &views,
            &state,
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
        );
    }
    Ok(())
}

/// Pick the plans to render based on the CLI flags. Errors with
fn select_plans(
    state: &RepoState,
    basename: &str,
    plan_arg: Option<&str>,
) -> anyhow::Result<Vec<PlanKey>> {
    if let Some(raw) = plan_arg {
        let stem = parse_arg(raw, basename)?;
        let key = PlanKey::parse(&stem)
            .map_err(|e| anyhow::anyhow!("invalid plan stem `{stem}`: {e}"))?;
        if !state.fold.plans.contains_key(&key) {
            anyhow::bail!(
                "plan `{basename}/{stem}.md` not active. active: {}",
                active_summary(state, basename)
            );
        }
        return Ok(vec![key]);
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
    let finished: Vec<serde_json::Value> = state
        .fold
        .finished_plans
        .iter()
        .map(|fp| {
            serde_json::json!({
                "plan": fp.plan.as_str(),
                "intro": fp.intro.as_str(),
                "finalized_at": fp.finalized_at.as_str(),
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
    })
}

fn print_human(
    repo: &Path,
    branch: Option<&str>,
    head_sha: Option<&str>,
    worktree_dirty: bool,
    views: &[&PlanWorkState],
    state: &RepoState,
) {
    println!("repo:   {}", repo.display());
    if let Some(b) = branch {
        println!("branch: {b}");
    }
    if let Some(s) = head_sha {
        println!("head:   {s}");
    }
    println!("dirty:  {}", if worktree_dirty { "yes" } else { "no" });

    if views.is_empty() && state.fold.plans.is_empty() {
        println!();
        println!("no active plan, nothing pending");
    }
    for v in views {
        println!();
        println!("plan: {}", v.plan.as_str());
        println!("  latest reviewable: {}", short_sha(v.sha.as_str()));
        println!("  gate:              {}", v.gate);
        println!("  waiting on:        {}", waiting_actor(&v.waiting_on));
        println!("  reason:            {}", waiting_reason(&v.waiting_on));
    }

    if !state.fold.finished_plans.is_empty() {
        println!();
        println!("finished plans:");
        for fp in &state.fold.finished_plans {
            println!(
                "  {} (finalized {})",
                fp.plan.as_str(),
                short_sha(fp.finalized_at.as_str())
            );
        }
    }
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
