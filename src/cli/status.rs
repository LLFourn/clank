//! `trinity status` — read-only summary of the current repo's
//! Trinity state. Fully local: folds the repo, projects via
//! `responses::list_plans_response`, reads HEAD + branch + dirty
//! state via `git`. No daemon.

use std::path::Path;

use super::{StatusArgs, repo_basename, resolve_repo};
use trinity_core::api::{PlanRow, StatusResponse};
use trinity_core::vocab::PlanLifecycle;

pub async fn run(args: StatusArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;
    let state = crate::rebuild::rebuild_repo(&repo)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
    let plans = crate::responses::list_plans_response(&state)?;

    let (current_branch, head_sha, head_subject) = head_info(&repo);
    let worktree_dirty = worktree_dirty(&repo)?;

    let response = StatusResponse {
        repo_root: repo.display().to_string(),
        repo_basename: basename,
        current_branch,
        head_sha,
        head_subject,
        worktree_dirty,
        plans,
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        render_human(&response);
    }
    Ok(())
}

/// `symbolic-ref --short HEAD` failing means detached HEAD (or no
/// commits yet) — that's a normal state we render as `(detached)`.
/// `log -1` failing only happens in an empty repo, where `None`s
/// are also a normal render. Genuine git invocation errors (binary
/// missing, permission denied) are vanishingly rare for an
/// already-resolved repo root and would surface as `None` here;
/// `worktree_dirty` is the only signal where swallowing a real
/// failure would be misleading, so it's `Result` below.
fn head_info(repo: &Path) -> (Option<String>, Option<String>, Option<String>) {
    let branch = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    let log = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "-1", "--format=%H%n%s"])
        .output()
        .ok();
    let (sha, subject) = match log {
        Some(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).into_owned();
            let mut lines = s.lines();
            let sha = lines.next().map(str::to_string).filter(|s| !s.is_empty());
            let subj = lines.next().map(str::to_string);
            (sha, subj)
        }
        _ => (None, None),
    };
    (branch, sha, subject)
}

fn worktree_dirty(repo: &Path) -> anyhow::Result<bool> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git status --porcelain failed (exit {}): {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim(),
        );
    }
    Ok(!out.stdout.iter().all(|b| b.is_ascii_whitespace()))
}

fn render_human(s: &StatusResponse) {
    let head_line = match (&s.head_sha, &s.head_subject) {
        (Some(sha), Some(subj)) => {
            format!(" @ {} {}", short_sha(sha), subj.trim())
        }
        (Some(sha), None) => format!(" @ {}", short_sha(sha)),
        _ => String::new(),
    };
    let branch = s.current_branch.as_deref().unwrap_or("(detached)");
    let dirty_mark = if s.worktree_dirty { "  (dirty)" } else { "" };
    println!(
        "repo  {} ({}{}){}",
        s.repo_root, branch, head_line, dirty_mark
    );

    let active: Vec<&PlanRow> = s
        .plans
        .plans
        .iter()
        .filter(|p| p.lifecycle == PlanLifecycle::Active)
        .collect();
    let finished: Vec<&PlanRow> = s
        .plans
        .plans
        .iter()
        .filter(|p| p.lifecycle == PlanLifecycle::Finished)
        .collect();

    if !active.is_empty() {
        println!();
        println!("active plans:");
        for row in &active {
            print_row(row);
        }
    }
    if !finished.is_empty() {
        println!();
        println!("finished plans:");
        for row in &finished {
            println!("  {}", row.plan_id.as_deref().unwrap_or(row.slug.as_str()));
        }
    }
    if active.is_empty() && finished.is_empty() {
        println!();
        println!("no plans yet — `trinity start_plan` via your agent's MCP to create one.");
    }

    if !s.plans.conflicts.is_empty() {
        println!();
        println!("conflicts:");
        for c in &s.plans.conflicts {
            let summary = serde_json::to_string(c).unwrap_or_default();
            println!("  {summary}");
        }
    }
}

fn print_row(row: &PlanRow) {
    let id = row.plan_id.as_deref().unwrap_or(row.slug.as_str());
    println!("  {id}");
    println!("    phase:    {}", row.phase.as_str());
    println!("    waiting:  {}", waiting_summary(&row.waiting_on));
    println!("    path:     {}", row.current_path);
    if let Some(sha) = &row.latest_reviewable_sha {
        println!("    latest:   {}", short_sha(sha.as_str()));
    }
    match row.gate_state {
        Some(s) => println!("    gate:     {}", s.as_str()),
        None => println!("    gate:     (no reviewable commit yet)"),
    }
    if row.plan_worktree_status != trinity_core::vocab::PlanWorktreeStatus::Clean {
        println!("    worktree: {:?}", row.plan_worktree_status);
    }
}

fn waiting_summary(w: &trinity_core::api::WaitingOn) -> String {
    let role = match w.role {
        trinity_core::vocab::WaitingRole::Master => "master",
        trinity_core::vocab::WaitingRole::Reviewers => "reviewers",
        trinity_core::vocab::WaitingRole::None => "none",
    };
    if w.agents.is_empty() {
        role.to_string()
    } else {
        let names: Vec<&str> = w.agents.iter().map(|a| a.as_str()).collect();
        format!("{role} ({})", names.join(", "))
    }
}

fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}
