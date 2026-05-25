//! `clank feedback write` — write a typed feedback file.
//!
//! Writes to `.clank/agents/<author>/feedback/<sha>.md`.

use std::io::Write;
use std::path::Path;

use anyhow::Context;

use super::{FeedbackCmd, FeedbackWriteArgs, resolve_repo};
use crate::disk_format::feedback_path_wire;
use crate::lifecycle::{AgentLabel, CommitRef, CommitSha};
use clank_core::ids::CommitRefResolveError;

pub async fn run(args: super::FeedbackArgs) -> anyhow::Result<()> {
    match args.command {
        FeedbackCmd::Write(write) => run_write(write).await,
    }
}

async fn run_write(args: FeedbackWriteArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;

    let commit_ref = CommitRef::parse(&args.commit)
        .map_err(|e| anyhow::anyhow!("invalid --commit `{}`: {e}", args.commit))?;
    let author = AgentLabel::parse(&args.author)
        .map_err(|e| anyhow::anyhow!("invalid --author `{}`: {e}", args.author))?;
    let expected_verdict: clank_core::Verdict = args.verdict.into();
    let verdict_header = match expected_verdict {
        clank_core::Verdict::Approve => "APPROVE",
        clank_core::Verdict::RequestChanges => "REQUEST_CHANGES",
        clank_core::Verdict::Unmarked => {
            anyhow::bail!("verdict `unmarked` cannot be written");
        }
    };

    let body = format!("{verdict_header} {}\n", args.message.trim());

    let state =
        crate::rebuild::rebuild_repo_with_policy(&repo, crate::rebuild::CachePolicy::Bypass)
            .await
            .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;

    let mut all_shas: Vec<CommitSha> = Vec::new();
    for ps in state.fold.plans.values() {
        all_shas.extend(ps.reviewable_shas());
    }
    for ah in &state.fold.ad_hoc {
        all_shas.push(ah.sha.clone());
    }
    all_shas.sort();
    all_shas.dedup();

    let target_sha = commit_ref
        .resolve_against(&all_shas)
        .map_err(|e| match e {
            CommitRefResolveError::Orphan => anyhow::anyhow!(
                "--commit `{}` did not match any known commit. \
                 Known shas: {}",
                args.commit,
                format_short_list(&all_shas),
            ),
            CommitRefResolveError::Ambiguous { matches } => anyhow::anyhow!(
                "--commit `{}` is ambiguous (matched {} commits): \
                 {}. Pass a longer prefix or the full SHA.",
                args.commit,
                matches.len(),
                format_short_list(&matches),
            ),
        })?;

    let wire_path = feedback_path_wire(&author, &target_sha, &all_shas);
    let abs_path = repo.join(&wire_path);

    if let Some(parent) = abs_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating `{}`", parent.display()))?;
    }

    write_atomic(&abs_path, body.as_bytes())
        .with_context(|| format!("writing `{}`", abs_path.display()))?;

    println!("{wire_path}");
    Ok(())
}

fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(".clank-feedback-")
        .suffix(".md.tmp")
        .tempfile_in(parent)?;
    tmp.write_all(contents)?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn format_short_list(shas: &[CommitSha]) -> String {
    if shas.is_empty() {
        return "(none — plan has no reviewable commits yet)".into();
    }
    shas.iter()
        .map(|s| s.as_str()[..7].to_string())
        .collect::<Vec<_>>()
        .join(", ")
}
