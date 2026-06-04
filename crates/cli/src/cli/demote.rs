//! `clank demote <plan>` — abort an in-flight plan: drop its
//! commits from history AND save the plan body back to the queue
//! (or stub) for re-attempt.
//!
//! Transactional: no filesystem changes until the rewrite engine
//! returns success. The pre-rewrite checks (dirty-tree refusal,
//! safety tiering, target-collision pre-check) all operate on
//! in-memory state; the queue/stub write + orphan feedback
//! cleanup only fire after the rewrite succeeds.
//!
//! `--into-branch <name>` is preview-only — writes the rewritten
//! chain to a fresh branch and leaves master + queue/stub +
//! feedback untouched. The full demote semantics only fire on
//! the in-place path.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::cli::rewrite::{RewriteOpts, run as run_rewrite};
use crate::rebuild::CachePolicy;
use clank_core::api::{RewriteCommit, RewriteDisposition};
use clank_core::ids::CommitSha;

use super::DemoteArgs;

pub async fn run(args: DemoteArgs) -> anyhow::Result<()> {
    let repo = super::resolve_repo(args.repo.as_deref())?;
    let basename = crate::lifecycle::RepoBasename::from_repo_root(&repo)
        .ok_or_else(|| anyhow::anyhow!("unknown repo basename: {}", repo.display()))?;

    let state = crate::rebuild::rebuild_repo_with_policy(&repo, CachePolicy::Use)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
    let plan_key =
        crate::cli::plan_resolve::resolve_plan(&state, basename.as_str(), args.plan.as_deref())?;
    let stem = plan_key.as_str().to_string();

    let preview = crate::preview::build_rewrite_preview(&repo, &state, &plan_key, true)
        .await
        .map_err(|e| anyhow::anyhow!("rewrite preview failed: {e}"))?;

    // ── Step 3a: tiered safety check ──────────────────────────
    safety_check(&preview.commits, args.force)?;

    // ── Step 3b: dirty-tree refusal on the plan file ──────────
    let plan_rel = format!(".clank/plans/{stem}.md");
    if plan_file_dirty(&repo, &plan_rel, preview.head_sha.as_str())? {
        anyhow::bail!(
            "`{plan_rel}` in the working tree differs from HEAD. \
             Commit your plan-body changes or `git stash` before demoting."
        );
    }

    // ── Step 3c: read plan body from HEAD into memory ─────────
    let plan_body = crate::git_io::show_blob(&repo, &preview.head_sha, Path::new(&plan_rel))
        .context(format!(
            "reading `{plan_rel}` from HEAD ({}) into memory",
            preview.head_sha.as_str()
        ))?;

    // ── Step 3d: determine target path + collision pre-check ──
    let target = target_path(&repo, &stem, args.stub, args.priority);
    if target.exists() {
        anyhow::bail!(
            "target file `{}` already exists; pick a different priority or move the existing file",
            target.strip_prefix(&repo).unwrap_or(&target).display()
        );
    }

    // ── Step 3e: confirmation prompt (skipped on --dry / --yes) ──
    if !args.dry && !args.yes && !confirm_demote(&stem, args.into_branch.as_deref())? {
        anyhow::bail!("aborted");
    }

    // ── Step 3f: transform dispositions to all-Drop for plan-attributed commits ──
    // Foreign commits are gated out by the safety check above so
    // we never see them here under non-`--force` paths; under
    // `--force`, `KeepVerbatim` is still gated (unconditional
    // refusal) so we preserve them verbatim regardless.
    let demote_commits: Vec<RewriteCommit> = preview
        .commits
        .iter()
        .map(|c| {
            if c.foreign {
                c.clone()
            } else {
                RewriteCommit {
                    sha: c.sha.clone(),
                    subject: c.subject.clone(),
                    disposition: RewriteDisposition::Drop,
                    foreign: false,
                    strip_paths: c.strip_paths.clone(),
                }
            }
        })
        .collect();

    // ── Step 4: invoke rewrite engine ─────────────────────────
    let outcome = run_rewrite(RewriteOpts {
        repo: &repo,
        intro_sha: preview.intro_sha.as_ref(),
        head_sha: &preview.head_sha,
        linear: preview.linear,
        commits: &demote_commits,
        into_branch: args.into_branch.as_deref(),
        dry: args.dry,
        allow_rewrite_protected: args.allow_rewrite_protected,
        squash: None,
        head_strip_paths: &preview.head_strip_paths,
    })
    .await?;

    if args.dry {
        println!("dry-run: would demote `{stem}`");
        println!(
            "  plan body → {}",
            target.strip_prefix(&repo).unwrap_or(&target).display()
        );
        let orphan_count = count_orphan_feedback(&repo, &dropped_shas(&preview.commits))?;
        println!("  orphan feedback files to remove: {orphan_count}");
        return Ok(());
    }

    if args.into_branch.is_some() {
        if let (Some(tip), Some(branch)) = (outcome.new_tip, outcome.updated_branch) {
            println!(
                "rewrote `{stem}` onto `{branch}` (now at {} = {tip}).",
                &tip[..tip.len().min(7)],
            );
            println!(
                "To complete the demote: switch to `{branch}` and run `clank demote {stem}` (without --into-branch)."
            );
        }
        return Ok(());
    }

    // ── Step 5: post-rewrite writes (only on in-place success) ──
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating `{}`", parent.display()))?;
    }
    std::fs::write(&target, &plan_body)
        .with_context(|| format!("writing plan body to `{}`", target.display()))?;

    let dropped = dropped_shas(&preview.commits);
    let orphan_count = clean_orphan_feedback(&repo, &dropped)?;

    if let (Some(tip), Some(branch)) = (outcome.new_tip, outcome.updated_branch) {
        println!(
            "demoted `{stem}`: branch `{branch}` now at {} (dropped {} commits)",
            &tip[..tip.len().min(7)],
            dropped.len(),
        );
    } else {
        println!("demoted `{stem}`: dropped {} commits", dropped.len());
    }
    println!(
        "  plan body → {}",
        target.strip_prefix(&repo).unwrap_or(&target).display()
    );
    println!("  orphan feedback files removed: {orphan_count}");
    Ok(())
}

/// Tiered safety check per the plan:
/// - All `Drop`: ok.
/// - Any `Rewrite` (non-foreign): refuse unless `--force`.
/// - Any `KeepVerbatim` (foreign): unconditional refusal.
fn safety_check(commits: &[RewriteCommit], force: bool) -> anyhow::Result<()> {
    let mut rewrite_shas: Vec<&CommitSha> = Vec::new();
    let mut foreign_shas: Vec<&CommitSha> = Vec::new();
    for c in commits {
        match c.disposition {
            RewriteDisposition::Drop => {}
            RewriteDisposition::Rewrite if !c.foreign => rewrite_shas.push(&c.sha),
            RewriteDisposition::KeepVerbatim if c.foreign => foreign_shas.push(&c.sha),
            _ => {}
        }
    }
    if !foreign_shas.is_empty() {
        let list: Vec<String> = foreign_shas
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();
        anyhow::bail!(
            "demote refuses: foreign commit(s) interleaved in plan range: {}. \
             `--force` does NOT bypass this. Resolve via `git rebase -i` or \
             coordinate with the foreign-commit author(s).",
            list.join(", ")
        );
    }
    if !rewrite_shas.is_empty() && !force {
        let list: Vec<String> = rewrite_shas
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();
        anyhow::bail!(
            "demote refuses: commit(s) {} touch non-plan content that would be dropped. \
             Pass `--force` to drop them anyway (you opt in to losing those changes).",
            list.join(", ")
        );
    }
    Ok(())
}

/// True if `rel_path` in the working tree differs from its content
/// at `head_sha`. Missing-in-WT-but-present-in-HEAD also counts as
/// dirty (you should `git checkout` or commit the deletion first).
fn plan_file_dirty(repo: &Path, rel_path: &str, head_sha: &str) -> anyhow::Result<bool> {
    let wt_path = repo.join(rel_path);
    let wt_content = match std::fs::read_to_string(&wt_path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e).with_context(|| format!("reading `{}`", wt_path.display()));
        }
    };
    let head_sha_parsed = CommitSha::parse(head_sha)
        .map_err(|e| anyhow::anyhow!("invalid head SHA `{head_sha}`: {e}"))?;
    let head_content = match crate::git_io::show_blob(repo, &head_sha_parsed, Path::new(rel_path)) {
        Ok(s) => Some(s),
        Err(_) => None,
    };
    Ok(wt_content != head_content)
}

fn target_path(repo: &Path, stem: &str, stub: bool, priority: Option<u16>) -> PathBuf {
    if stub {
        repo.join(format!(".clank/stubs/{stem}.md"))
    } else {
        let n = priority.unwrap_or(500);
        repo.join(format!(".clank/queue/{n:03}-{stem}.md"))
    }
}

fn dropped_shas(commits: &[RewriteCommit]) -> Vec<CommitSha> {
    commits
        .iter()
        .filter(|c| !c.foreign)
        .map(|c| c.sha.clone())
        .collect()
}

/// Walk `.clank/agents/*/feedback/**` and delete files whose file
/// stem matches one of the dropped SHAs. Returns the count of
/// files removed.
fn clean_orphan_feedback(repo: &Path, dropped: &[CommitSha]) -> anyhow::Result<usize> {
    use std::collections::HashSet;
    let dropped_set: HashSet<&str> = dropped.iter().map(|s| s.as_str()).collect();
    let agents_root = repo.join(".clank/agents");
    if !agents_root.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    for agent_entry in std::fs::read_dir(&agents_root)? {
        let agent_dir = agent_entry?.path();
        let feedback_dir = agent_dir.join("feedback");
        if !feedback_dir.exists() {
            continue;
        }
        walk_and_remove_matching(&feedback_dir, &dropped_set, &mut count)?;
    }
    Ok(count)
}

fn count_orphan_feedback(repo: &Path, dropped: &[CommitSha]) -> anyhow::Result<usize> {
    use std::collections::HashSet;
    let dropped_set: HashSet<&str> = dropped.iter().map(|s| s.as_str()).collect();
    let agents_root = repo.join(".clank/agents");
    if !agents_root.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    for agent_entry in std::fs::read_dir(&agents_root)? {
        let agent_dir = agent_entry?.path();
        let feedback_dir = agent_dir.join("feedback");
        if !feedback_dir.exists() {
            continue;
        }
        walk_and_count_matching(&feedback_dir, &dropped_set, &mut count)?;
    }
    Ok(count)
}

fn walk_and_remove_matching(
    dir: &Path,
    targets: &std::collections::HashSet<&str>,
    count: &mut usize,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            walk_and_remove_matching(&path, targets, count)?;
        } else if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            if targets.contains(stem) {
                std::fs::remove_file(&path)?;
                *count += 1;
            }
        }
    }
    Ok(())
}

fn walk_and_count_matching(
    dir: &Path,
    targets: &std::collections::HashSet<&str>,
    count: &mut usize,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            walk_and_count_matching(&path, targets, count)?;
        } else if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            if targets.contains(stem) {
                *count += 1;
            }
        }
    }
    Ok(())
}

fn confirm_demote(stem: &str, into_branch: Option<&str>) -> anyhow::Result<bool> {
    let prompt = if let Some(branch) = into_branch {
        format!("Demote `{stem}` (preview-only — rewrites to branch `{branch}`)? [y/N] ")
    } else {
        format!("Demote `{stem}`? This will drop the plan's commits from history. [y/N] ")
    };
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes"))
}
