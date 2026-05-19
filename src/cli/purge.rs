//! `trinity purge` — strip a plan's `.trinity/` artifacts from
//! history.
//!
//! The CLI fetches `rewrite_preview` from the daemon and feeds the
//! typed manifest to the shared rewrite engine (`cli::rewrite`).
//! All git plumbing lives there.

use std::io::Write;

use super::{PurgeArgs, repo_basename, resolve_repo};
use crate::cli::rewrite::{RewriteOpts, run as run_rewrite};
use trinity_core::api::RewritePreviewResponse;

pub async fn run(args: PurgeArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;
    let stem = super::finish::resolve_stem_for_purge(&args.plan, &basename)?;
    let daemon = args.daemon.trim_end_matches('/').to_string();

    if args.amend {
        anyhow::bail!("--amend is not yet implemented in this phase");
    }
    if args.squash.is_some() {
        anyhow::bail!("--squash is not yet implemented in this phase");
    }
    if args.drop_finalize {
        anyhow::bail!("--drop-finalize is not yet implemented in this phase");
    }

    let preview = fetch_rewrite_preview(&daemon, &basename, &stem, args.drop_finalize).await?;

    if !args.dry && !args.yes && !confirm(&stem, args.into_branch.as_deref())? {
        anyhow::bail!("aborted");
    }

    let outcome = run_rewrite(RewriteOpts {
        repo: &repo,
        preview: &preview,
        into_branch: args.into_branch.as_deref(),
        dry: args.dry,
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

fn confirm(stem: &str, into_branch: Option<&str>) -> anyhow::Result<bool> {
    let target = match into_branch {
        Some(b) => format!("a NEW branch `{b}` (current branch left untouched)"),
        None => "the CURRENT branch (history will be rewritten in place)".into(),
    };
    print!(
        "About to purge `{stem}` and write rewritten history to {target}. \
         Continue? [y/N] "
    );
    std::io::stdout().flush()?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(matches!(buf.trim().to_lowercase().as_str(), "y" | "yes"))
}
