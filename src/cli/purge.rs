//! `trinity purge` — strip a plan's `.trinity/` artifacts from
//! history.
//!
//! The CLI fetches `rewrite_preview` from the daemon and feeds the
//! typed manifest to the shared rewrite engine (`cli::rewrite`).
//! All git plumbing lives there.

use std::io::Write;

use super::{PurgeArgs, repo_basename, resolve_repo};
use crate::cli::rewrite::{RewriteOpts, run as run_rewrite};
use trinity_core::api::{PurgeAllPreviewResponse, RewritePreviewResponse};

pub async fn run(args: PurgeArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;
    let daemon = args.daemon.trim_end_matches('/').to_string();

    if args.all && args.plan.is_some() {
        anyhow::bail!("--all and a plan argument are mutually exclusive; pass one or the other",);
    }
    if args.squash.is_some() && args.all {
        // Squashing every plan's history into one commit would
        // lose per-plan boundaries; refuse rather than try.
        anyhow::bail!("--squash is not supported with --all");
    }
    if args.amend && args.squash.is_some() {
        anyhow::bail!("--amend and --squash are mutually exclusive");
    }
    if args.amend && args.into_branch.is_some() {
        anyhow::bail!("--amend and --into-branch are mutually exclusive");
    }

    if args.amend {
        return run_amend(&repo, &basename, &daemon, &args).await;
    }

    if args.all {
        run_all(&repo, &basename, &daemon, &args).await
    } else {
        run_single(&repo, &basename, &daemon, &args).await
    }
}

async fn run_single(
    repo: &std::path::Path,
    basename: &str,
    daemon: &str,
    args: &PurgeArgs,
) -> anyhow::Result<()> {
    let stem = super::finish::resolve_stem_or_infer_for_purge(&args.plan, basename, daemon).await?;
    // Always strip the finalize snapshot too — if anyone wants to
    // preserve the audit trail, we can add `--keep-finalize` later.
    let preview = fetch_rewrite_preview(daemon, basename, &stem, true).await?;

    if !args.dry && !args.yes && !confirm_single(&stem, args.into_branch.as_deref())? {
        anyhow::bail!("aborted");
    }

    let outcome = run_rewrite(RewriteOpts {
        repo,
        intro_sha: preview.intro_sha.as_ref(),
        head_sha: &preview.head_sha,
        linear: preview.linear,
        commits: &preview.commits,
        into_branch: args.into_branch.as_deref(),
        dry: args.dry,
        allow_rewrite_protected: args.allow_rewrite_protected,
        squash: args.squash.as_deref(),
        head_strip_paths: &preview.head_strip_paths,
    })
    .await?;

    if args.dry {
        return Ok(());
    }
    if let (Some(tip), Some(branch)) = (outcome.new_tip, outcome.updated_branch) {
        println!(
            "rewrote `{stem}`: branch `{branch}` now at {} ({})",
            &tip[..tip.len().min(7)],
            tip,
        );
    }
    Ok(())
}

async fn run_all(
    repo: &std::path::Path,
    basename: &str,
    daemon: &str,
    args: &PurgeArgs,
) -> anyhow::Result<()> {
    let preview = fetch_all_preview(daemon, basename).await?;
    if preview.intro_sha.is_none() {
        println!("no .trinity/ history found in `{basename}`; nothing to purge.");
        return Ok(());
    }

    // The all-plans warning is loud — log to stderr even when
    // `--yes` skips the prompt so script-driven invocations still
    // produce an audit trail.
    let warning = format_all_warning(preview.plans_touched.len(), args.into_branch.as_deref());
    eprintln!("{warning}");
    if !args.dry && !args.yes && !confirm_with(&warning)? {
        anyhow::bail!("aborted");
    }

    let outcome = run_rewrite(RewriteOpts {
        repo,
        intro_sha: preview.intro_sha.as_ref(),
        head_sha: &preview.head_sha,
        linear: preview.linear,
        commits: &preview.commits,
        into_branch: args.into_branch.as_deref(),
        dry: args.dry,
        allow_rewrite_protected: args.allow_rewrite_protected,
        squash: args.squash.as_deref(),
        head_strip_paths: &preview.head_strip_paths,
    })
    .await?;

    if args.dry {
        return Ok(());
    }
    if let (Some(tip), Some(branch)) = (outcome.new_tip, outcome.updated_branch) {
        println!(
            "purged all .trinity/ history: branch `{branch}` now at {} ({})",
            &tip[..tip.len().min(7)],
            tip,
        );
    }
    Ok(())
}

/// `--amend`: strip the plan's `.trinity/` paths from HEAD's tree
/// and amend HEAD (no chain rewrite). Useful when the operator
/// has just landed a commit and wants to retroactively scrub the
/// plan's artifacts from HEAD's tree without rewriting earlier
/// history.
async fn run_amend(
    repo: &std::path::Path,
    basename: &str,
    daemon: &str,
    args: &PurgeArgs,
) -> anyhow::Result<()> {
    let stem = if args.all {
        None
    } else {
        Some(super::finish::resolve_stem_or_infer_for_purge(&args.plan, basename, daemon).await?)
    };

    // Plan rule: `--amend` requires HEAD to be a finalize commit
    // for the named plan (or for any plan under `--all`). HEAD is
    // a finalize commit iff every changed path lies under
    // `.trinity/finished/<stem>/` (single-plan) or
    // `.trinity/finished/` (all-plans).
    let head_files = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"])
        .output()?;
    let head_lines: Vec<String> = if head_files.status.success() {
        String::from_utf8_lossy(&head_files.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };
    let prefix: String = match stem.as_deref() {
        Some(s) => format!(".trinity/finished/{s}/"),
        None => ".trinity/finished/".to_string(),
    };
    let head_is_finalize =
        !head_lines.is_empty() && head_lines.iter().all(|l| l.starts_with(&prefix));
    if !head_is_finalize {
        anyhow::bail!(
            "--amend requires HEAD to be a finalize commit \
             (every changed path under `{prefix}`). Run `trinity finish` \
             without `--amend` to create the finalize commit first, or \
             use `trinity purge` without `--amend` to rewrite the chain."
        );
    }

    // Get HEAD sha and the strippable set in HEAD's tree.
    let head_sha = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()?;
    if !head_sha.status.success() {
        anyhow::bail!("git rev-parse HEAD failed");
    }
    let head_sha_str = String::from_utf8(head_sha.stdout)?.trim().to_string();

    let strip_paths: Vec<String> = match stem.as_deref() {
        Some(s) => {
            // Single-plan: ls-tree for this plan's paths
            // (always include finalize).
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args([
                    "ls-tree",
                    "-r",
                    "--name-only",
                    "--",
                    &head_sha_str,
                    &format!(".trinity/plans/{s}.md"),
                    &format!(".trinity/finished/{s}/"),
                ])
                .output()?;
            if !out.status.success() {
                Vec::new()
            } else {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect()
            }
        }
        None => {
            // All-plans: every `.trinity/` path in HEAD's tree.
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args([
                    "ls-tree",
                    "-r",
                    "--name-only",
                    "--",
                    &head_sha_str,
                    ".trinity/",
                ])
                .output()?;
            if !out.status.success() {
                Vec::new()
            } else {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect()
            }
        }
    };

    if strip_paths.is_empty() {
        println!("HEAD's tree has no strippable Trinity paths; nothing to amend");
        return Ok(());
    }

    // Pre-flights mirror the engine's: dirty worktree and
    // protected-branch refusal. --amend mutates HEAD in place, so
    // it gets the same safety constraints as in-place rewrite.
    let status_out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()?;
    if !status_out.status.success()
        || !String::from_utf8_lossy(&status_out.stdout)
            .trim()
            .is_empty()
    {
        anyhow::bail!("working tree dirty; commit or stash first");
    }
    if !args.allow_rewrite_protected {
        let current = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["symbolic-ref", "--short", "HEAD"])
            .output()?;
        let branch = String::from_utf8_lossy(&current.stdout).trim().to_string();
        let protected_by_name = matches!(branch.as_str(), "main" | "master");
        let protected_by_config = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["config", "--get", &format!("branch.{branch}.protect")])
            .output()
            .map(|o| {
                o.status.success()
                    && matches!(
                        String::from_utf8_lossy(&o.stdout).trim(),
                        "true" | "1" | "yes" | "on"
                    )
            })
            .unwrap_or(false);
        if protected_by_name || protected_by_config {
            anyhow::bail!(
                "refusing to amend protected branch `{branch}`. \
                 Pass `--allow-rewrite-protected` to override."
            );
        }
    }

    if args.dry {
        println!("# trinity purge --amend preview");
        println!("# HEAD: {head_sha_str}");
        println!("# would strip from HEAD's tree:");
        for p in &strip_paths {
            println!("#   - {p}");
        }
        println!("# (--dry: no commits, no refs touched)");
        return Ok(());
    }

    if !args.yes
        && !confirm_with(&format!(
            "About to amend HEAD: strip {} path(s) from HEAD's tree.",
            strip_paths.len()
        ))?
    {
        anyhow::bail!("aborted");
    }

    // Remove each path from the index and amend.
    for p in &strip_paths {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rm", "--cached", "-q", p])
            .status()?;
        if !status.success() {
            anyhow::bail!("git rm --cached {p} failed");
        }
    }
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "--amend", "--no-edit", "--allow-empty"])
        .status()?;
    if !status.success() {
        anyhow::bail!("git commit --amend failed");
    }
    println!("amended HEAD: stripped {} path(s)", strip_paths.len());
    Ok(())
}

async fn fetch_all_preview(
    daemon: &str,
    basename: &str,
) -> anyhow::Result<PurgeAllPreviewResponse> {
    let url = format!("{daemon}/api/repos/{basename}/rewrite_preview_all?include_finalize=true");
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("daemon unreachable at {daemon}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("daemon returned {status} for {url}: {body}");
    }
    Ok(resp.json::<PurgeAllPreviewResponse>().await?)
}

async fn fetch_rewrite_preview(
    daemon: &str,
    basename: &str,
    stem: &str,
    include_finalize: bool,
) -> anyhow::Result<RewritePreviewResponse> {
    let url = format!(
        "{daemon}/api/plan/{basename}/{stem}.md/rewrite_preview?include_finalize={include_finalize}",
    );
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("daemon unreachable at {daemon}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("daemon returned {status} for {url}: {body}");
    }
    Ok(resp.json::<RewritePreviewResponse>().await?)
}

fn confirm_single(stem: &str, into_branch: Option<&str>) -> anyhow::Result<bool> {
    let target = match into_branch {
        Some(b) => format!("a NEW branch `{b}` (current branch left untouched)"),
        None => "the CURRENT branch (history will be rewritten in place)".into(),
    };
    let prompt = format!(
        "About to purge `{stem}` and write rewritten history to {target}. \
         Continue? [y/N] "
    );
    confirm_with(&prompt)
}

/// Render the all-plans confirmation banner. Logged on stderr
/// before the engine runs even when `--yes` skips the interactive
/// prompt — script-driven invocations should still see an audit
/// trail showing this was a destructive operation.
fn format_all_warning(plan_count: usize, into_branch: Option<&str>) -> String {
    let target = match into_branch {
        Some(b) => format!("a NEW branch `{b}` (current branch left untouched)"),
        None => "the CURRENT branch (history will be rewritten in place)".into(),
    };
    let noun = if plan_count == 1 { "plan" } else { "plans" };
    format!(
        "About to purge EVERY `.trinity/` path from history \
         ({plan_count} {noun} touched in the range) and write \
         rewritten history to {target}."
    )
}

fn confirm_with(banner: &str) -> anyhow::Result<bool> {
    print!(
        "{banner}\n\
         Continue? [y/N] "
    );
    std::io::stdout().flush()?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(matches!(buf.trim().to_lowercase().as_str(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn all_with_plan_arg_errors_before_http() {
        let args = PurgeArgs {
            plan: Some("foo".into()),
            all: true,
            repo: Some(std::env::current_dir().unwrap()),
            // Bogus daemon URL — if we reached HTTP we'd see a connection
            // refused; the mutual-exclusion check should fire first.
            daemon: "http://127.0.0.1:1".into(),
            into_branch: None,
            dry: false,
            yes: true,
            squash: None,
            amend: false,
            allow_rewrite_protected: false,
        };
        let err = run(args).await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("mutually exclusive"),
            "expected mutual-exclusion error, got: {msg}"
        );
    }

    #[test]
    fn all_warning_includes_plan_count_and_target() {
        let into_branch = format_all_warning(7, Some("scrubbed"));
        assert!(into_branch.contains("EVERY"), "got: {into_branch}");
        assert!(into_branch.contains("7 plans"), "got: {into_branch}");
        assert!(
            into_branch.contains("NEW branch `scrubbed`"),
            "got: {into_branch}"
        );

        let in_place = format_all_warning(3, None);
        assert!(in_place.contains("CURRENT branch"), "got: {in_place}");
        assert!(in_place.contains("rewritten in place"), "got: {in_place}");
    }

    #[test]
    fn all_warning_singularizes_one_plan() {
        let msg = format_all_warning(1, None);
        assert!(msg.contains("1 plan touched"), "got: {msg}");
        assert!(!msg.contains("1 plans"), "got: {msg}");
    }
}
