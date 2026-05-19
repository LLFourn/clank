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

    if args.amend {
        anyhow::bail!("--amend is not yet implemented in this phase");
    }
    if args.squash.is_some() && args.all {
        // Squashing every plan's history into one commit would
        // lose per-plan boundaries; refuse rather than try.
        anyhow::bail!("--squash is not supported with --all");
    }

    if args.all && args.plan.is_some() {
        anyhow::bail!("--all and a plan argument are mutually exclusive; pass one or the other",);
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
