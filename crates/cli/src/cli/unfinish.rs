//! `clank unfinish <plan>` — drop the finish commit from
//! history. Rewrites HEAD, doesn't layer an inverse commit.

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

    require_clean_worktree(&repo)?;
    require_head_is_trivial_finish(&repo, plan_key.as_str())?;

    let dropped = head_sha(&repo)?;

    crate::git_plumbing::reset_hard(&repo, "HEAD~")?;

    println!(
        "unfinished `{}`; dropped finalize commit {}",
        plan_key.as_str(),
        short(&dropped)
    );
    Ok(())
}

/// Bail if the worktree or index has any changes. `git reset
/// --hard HEAD~` would otherwise obliterate them.
fn require_clean_worktree(repo: &Path) -> anyhow::Result<()> {
    let dirty = crate::git_io::working_tree_dirty_paths(repo)?;
    if !dirty.is_empty() {
        anyhow::bail!(
            "worktree or index is dirty — commit or stash your changes before running `clank unfinish`.\n\n{}",
            dirty.join("\n")
        );
    }
    Ok(())
}

/// Bail unless HEAD is a trivial finish commit for `<stem>` —
/// exactly two path changes that rename
/// `.clank/plans/<stem>.md` → `.clank/finished/<stem>.md`
/// (delete the former, add the latter; nothing else). Anything
/// else and we refuse to rewrite history, since `git reset
/// --hard HEAD~` would silently drop the extra changes.
fn require_head_is_trivial_finish(repo: &Path, stem: &str) -> anyhow::Result<()> {
    let plans_rel = crate::init_facts::plan_md_rel(stem);
    let finished_rel = crate::init_facts::finished_md_rel(stem);

    let lines = crate::git_io::diff_tree_name_status(repo, false)?;
    let mut entries: Vec<(char, String)> = Vec::new();
    for line in &lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, '\t');
        let status = parts
            .next()
            .and_then(|s| s.chars().next())
            .ok_or_else(|| anyhow::anyhow!("malformed diff-tree line: `{line}`"))?;
        let path = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("malformed diff-tree line: `{line}`"))?
            .to_string();
        entries.push((status, path));
    }

    let added_finished = entries.iter().any(|(s, p)| *s == 'A' && p == &finished_rel);
    let deleted_plan = entries.iter().any(|(s, p)| *s == 'D' && p == &plans_rel);
    if !added_finished {
        anyhow::bail!(
            "finish for `{stem}` is not at HEAD — `{finished_rel}` was not added by this commit. Find the finish commit and operate from there."
        );
    }
    if !deleted_plan {
        anyhow::bail!(
            "HEAD added `{finished_rel}` but did not delete `{plans_rel}` — this isn't a trivial finish commit. Fix the history manually first."
        );
    }

    let extras: Vec<&(char, String)> = entries
        .iter()
        .filter(|(s, p)| !((*s == 'A' && p == &finished_rel) || (*s == 'D' && p == &plans_rel)))
        .collect();
    if !extras.is_empty() {
        let listed = extras
            .iter()
            .map(|(s, p)| format!("{s}\t{p}"))
            .collect::<Vec<_>>()
            .join("\n  ");
        anyhow::bail!(
            "HEAD's finish commit carries changes beyond the rename — refusing to drop it. Fix the history manually first.\n  {listed}"
        );
    }

    // Content equality: HEAD~:plans/<stem>.md must match
    // HEAD:finished/<stem>.md byte-for-byte. A rename that
    // also edited content would silently lose the edit on
    // `git reset --hard HEAD~`.
    let before = crate::git_io::blob_at_rev(repo, "HEAD~", &plans_rel)?;
    let after = crate::git_io::blob_at_rev(repo, "HEAD", &finished_rel)?;
    if before != after {
        anyhow::bail!(
            "HEAD's finish commit changed plan body in addition to renaming it. Fix the history manually first."
        );
    }
    Ok(())
}

fn head_sha(repo: &Path) -> anyhow::Result<String> {
    Ok(crate::git_io::rev_parse_head(repo)?
        .ok_or_else(|| anyhow::anyhow!("git rev-parse HEAD failed: repository has no commits"))?
        .as_str()
        .to_string())
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}
