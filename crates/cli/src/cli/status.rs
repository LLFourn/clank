//! `clank status` — read-only summary of the current repo's Clank
//! state. Folds the repo, projects per-plan via `clank-core::plan_view`,
//! then renders. Never blocks.

use std::path::Path;

use super::{StatusArgs, repo_basename, resolve_repo};
use crate::cli::plan_resolve::parse_arg;
use crate::feedback_scan::scan_feedback;
use crate::lifecycle::PlanKey;
use crate::repo_state::RepoState;
use crate::worktree_facts::read_worktree_facts;
use clank_core::plan_view::{PlanView, WaitingOn, project};

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

    let selected = select_plans(&state, &basename, args.all, args.plan.as_deref())?;
    let mut views: Vec<PlanView> = Vec::with_capacity(selected.len());
    for key in &selected {
        if let Some(view) = build_view(&repo, &state, key).await? {
            views.push(view);
        }
    }

    if args.json {
        let json = build_json(
            &repo,
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
            args.all,
        );
    }
    Ok(())
}

/// Pick the plans to render based on the CLI flags. Errors with
/// exit-3 semantics when no flag is set and the repo has > 1
/// active plan.
fn select_plans(
    state: &RepoState,
    basename: &str,
    all: bool,
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
    if all {
        return Ok(state.fold.plans.keys().cloned().collect());
    }
    let actives: Vec<PlanKey> = state.fold.plans.keys().cloned().collect();
    match actives.as_slice() {
        [] => Ok(Vec::new()),
        [one] => Ok(vec![one.clone()]),
        _ => {
            let err = anyhow::anyhow!(
                "{} active plans in `{basename}`; pass --all or --plan <stem>. candidates: {}",
                actives.len(),
                active_summary(state, basename)
            );
            Err(err.context(ExitCode(3)))
        }
    }
}

/// Marker carried via `anyhow::Error::context` so `main` can map an
/// "ambiguous active plans" error to exit code 3 without coupling
/// the rest of the CLI to a custom error enum.
#[derive(Debug, Clone, Copy)]
pub struct ExitCode(pub i32);
impl std::fmt::Display for ExitCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit_code:{}", self.0)
    }
}

async fn build_view(
    repo: &Path,
    state: &RepoState,
    plan: &PlanKey,
) -> anyhow::Result<Option<PlanView>> {
    let ps = match state.fold.plans.get(plan) {
        Some(ps) => ps,
        None => return Ok(None),
    };
    let reviewable = ps.reviewable_shas();
    let feedback = scan_feedback(repo, plan, &reviewable)?;
    let plan_path = format!(".clank/plans/{}.md", plan.as_str());
    let worktree = read_worktree_facts(repo, &plan_path, state.head.as_ref()).await?;
    Ok(project(&state.fold, plan, &feedback, &worktree))
}

fn build_json(
    repo: &Path,
    basename: &str,
    branch: Option<String>,
    head_sha: Option<String>,
    head_subject: Option<String>,
    worktree_dirty: bool,
    views: &[PlanView],
    state: &RepoState,
) -> serde_json::Value {
    let plans: Vec<serde_json::Value> = views
        .iter()
        .map(|v| {
            serde_json::json!({
                "plan": v.plan.as_str(),
                "plan_path": format!(".clank/plans/{}.md", v.plan.as_str()),
                "latest_reviewable_sha": v.latest_reviewable_sha.as_str(),
                "gate_state": v.gate_state,
                "waiting_on": v.waiting_on,
                "worktree_status": v.worktree_status,
                "last_activity_ts": v.last_activity_ts,
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
                "plan_path": format!(".clank/plans/{}.md", fp.plan.as_str()),
                "intro": fp.intro.as_str(),
                "finalized_at": fp.finalized_at.as_str(),
            })
        })
        .collect();
    serde_json::json!({
        "repo_root": repo.display().to_string(),
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
    views: &[PlanView],
    state: &RepoState,
    all: bool,
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
        println!(
            "  latest reviewable: {}",
            short_sha(v.latest_reviewable_sha.as_str())
        );
        println!("  gate:              {}", v.gate_state);
        println!("  plan file:         {}", v.worktree_status);
        println!("  waiting on:        {}", waiting_actor(&v.waiting_on));
        println!("  reason:            {}", waiting_reason(&v.waiting_on));
    }

    if !state.fold.finished_plans.is_empty() {
        let total = state.fold.finished_plans.len();
        let show = if all { total } else { total.min(3) };
        println!();
        if show < total {
            println!("finished plans ({show} of {total}):");
        } else {
            println!("finished plans:");
        }
        for fp in state.fold.finished_plans.iter().rev().take(show).rev() {
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
