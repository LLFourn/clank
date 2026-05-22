//! `trinity status` — read-only summary of the current repo's
//! Trinity state. Fully local: folds the repo, projects locally,
//! reads HEAD + branch + dirty state via `git`. No daemon.

use std::path::Path;

use super::{StatusArgs, resolve_repo};

pub async fn run(args: StatusArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let policy = if args.no_cache {
        crate::rebuild::CachePolicy::Bypass
    } else {
        crate::rebuild::CachePolicy::Use
    };
    let state = crate::rebuild::rebuild_repo_with_policy(&repo, policy)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;

    let (current_branch, head_sha, head_subject) = head_info(&repo);
    let worktree_dirty = worktree_dirty(&repo)?;

    if args.json {
        let json = build_json(
            &state,
            &repo,
            current_branch,
            head_sha,
            head_subject,
            worktree_dirty,
        );
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        print_human(
            &state,
            &repo,
            current_branch.as_deref(),
            head_sha.as_deref(),
            worktree_dirty,
        );
    }
    Ok(())
}

fn build_json(
    state: &crate::repo_state::RepoState,
    repo: &Path,
    current_branch: Option<String>,
    head_sha: Option<String>,
    head_subject: Option<String>,
    worktree_dirty: bool,
) -> serde_json::Value {
    let mut active = Vec::new();
    let mut finished = Vec::new();
    for key in state.fold.plans.keys() {
        active.push(serde_json::json!({
            "slug": key.as_str(),
            "lifecycle": "active",
            "current_path": format!(".trinity/plans/{}.md", key.as_str()),
            "commit_count": state.fold.plans[key].commits.len(),
        }));
    }
    for fp in &state.fold.finished_plans {
        finished.push(serde_json::json!({
            "slug": fp.plan.as_str(),
            "lifecycle": "finished",
            "intro": fp.intro.as_str(),
            "finalized_at": fp.finalized_at.as_str(),
        }));
    }
    serde_json::json!({
        "repo_root": repo.display().to_string(),
        "current_branch": current_branch,
        "head_sha": head_sha,
        "head_subject": head_subject,
        "worktree_dirty": worktree_dirty,
        "active_plans": active,
        "finished_plans": finished,
    })
}

fn print_human(
    state: &crate::repo_state::RepoState,
    repo: &Path,
    current_branch: Option<&str>,
    head_sha: Option<&str>,
    worktree_dirty: bool,
) {
    println!("repo:   {}", repo.display());
    if let Some(b) = current_branch {
        println!("branch: {b}");
    }
    if let Some(s) = head_sha {
        println!("head:   {s}");
    }
    println!("dirty:  {}", if worktree_dirty { "yes" } else { "no" });

    let active: Vec<_> = state.fold.plans.keys().collect();
    if !active.is_empty() {
        println!();
        println!("active plans:");
        for key in active {
            let commits = state.fold.plans[key].commits.len();
            println!("  {} ({} commits)", key.as_str(), commits);
        }
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
