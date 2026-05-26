//! `clank unfinish <plan>` — move a finished plan back to active.

use std::path::Path;

use super::{UnfinishArgs, repo_basename, resolve_repo};

pub async fn run(args: UnfinishArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;

    let stem = match args.plan.as_deref() {
        Some(raw) => crate::cli::plan_resolve::parse_arg(raw, &basename)?,
        None => anyhow::bail!("plan argument is required for unfinish"),
    };
    let plan_key = clank_core::ids::PlanKey::parse(&stem)
        .map_err(|e| anyhow::anyhow!("invalid plan stem `{stem}`: {e}"))?;

    let finished_path = repo.join(format!(".clank/finished/{}.md", plan_key.as_str()));
    if !finished_path.exists() {
        anyhow::bail!(
            "plan `{}` is not finished (no `.clank/finished/{}.md`)",
            plan_key.as_str(),
            plan_key.as_str()
        );
    }

    let plans_path = repo.join(format!(".clank/plans/{}.md", plan_key.as_str()));
    if plans_path.exists() {
        anyhow::bail!(
            "`.clank/plans/{}.md` already exists — plan is already active",
            plan_key.as_str()
        );
    }

    std::fs::create_dir_all(repo.join(".clank/plans"))?;

    git_run(
        &repo,
        &[
            "mv",
            &format!(".clank/finished/{}.md", plan_key.as_str()),
            &format!(".clank/plans/{}.md", plan_key.as_str()),
        ],
    )?;

    let msg = format!("Unfinish {}", plan_key.as_str());
    git_run(&repo, &["commit", "--quiet", "-m", &msg])?;

    println!("unfinished `{}`", plan_key.as_str());
    Ok(())
}

fn git_run(repo: &Path, args: &[&str]) -> anyhow::Result<()> {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()?;
    if !status.success() {
        anyhow::bail!(
            "git {} failed (exit {})",
            args.join(" "),
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}
