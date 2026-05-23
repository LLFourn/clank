//! `clank feedback write` — write a typed feedback file.
//!
//! Wraps the file-write at
//! `.clank/agents/<author>/feedback/<plan>/<commit>.md` so:
//! - Codex gets a stable command prefix to one-shot approve
//!   (vs. re-prompting on every raw `apply_patch`).
//! - The verdict header is validated against the `--verdict`
//!   claim in one place (via `clank_core::feedback_body`).
//! - The commit ref is resolved against the plan's reviewable
//!   shas (short → long disambiguation; orphan refs are
//!   rejected up-front).
//! - The write is atomic (temp file + rename) so a crash
//!   mid-write can't leave a half-written feedback file the
//!   reviewer-scan would treat as `Unmarked`.

use std::io::{Read, Write};
use std::path::Path;

use anyhow::Context;

use super::{FeedbackCmd, FeedbackWriteArgs, repo_basename, resolve_repo};
use crate::cli::plan_resolve::parse_arg;
use crate::disk_format::{FeedbackTarget, feedback_path_wire};
use crate::lifecycle::{AgentLabel, CommitRef, CommitSha, PlanKey};
use clank_core::feedback_body::FeedbackBody;
use clank_core::ids::CommitRefResolveError;

pub async fn run(args: super::FeedbackArgs) -> anyhow::Result<()> {
    match args.command {
        FeedbackCmd::Write(write) => run_write(write).await,
    }
}

async fn run_write(args: FeedbackWriteArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;

    let stem = parse_arg(&args.plan, &basename)?;
    let plan =
        PlanKey::parse(&stem).map_err(|e| anyhow::anyhow!("invalid plan stem `{stem}`: {e}"))?;
    let commit_ref = CommitRef::parse(&args.commit)
        .map_err(|e| anyhow::anyhow!("invalid --commit `{}`: {e}", args.commit))?;
    let author = AgentLabel::parse(&args.author)
        .map_err(|e| anyhow::anyhow!("invalid --author `{}`: {e}", args.author))?;
    let expected_verdict: clank_core::Verdict = args.verdict.into();

    let body = read_body(&args.body_file)
        .with_context(|| format!("reading body from `{}`", args.body_file))?;

    FeedbackBody::parse(&body)
        .validate_matches(expected_verdict)
        .map_err(|e| anyhow::anyhow!("body validation failed: {e}"))?;

    // Write paths can't tolerate a stale fold: if a commit landed
    // since the last cache entry, the reviewable set won't contain
    // it and an otherwise-valid write would be rejected as orphan.
    // Bypass the cache for this branch.
    let state =
        crate::rebuild::rebuild_repo_with_policy(&repo, crate::rebuild::CachePolicy::Bypass)
            .await
            .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;

    let plan_state = state.fold.plans.get(&plan).ok_or_else(|| {
        anyhow::anyhow!(
            "plan `{}/{}.md` is not active in this repo. Feedback can only be \
             written against in-flight plans.",
            basename.as_str(),
            plan.as_str(),
        )
    })?;

    let reviewable = plan_state.reviewable_shas();

    let target_sha = commit_ref
        .resolve_against(&reviewable)
        .map_err(|e| match e {
            CommitRefResolveError::Orphan => anyhow::anyhow!(
                "--commit `{}` did not match any reviewable commit for plan `{}`. \
                 Reviewable shas: {}",
                args.commit,
                plan.as_str(),
                format_short_list(&reviewable),
            ),
            CommitRefResolveError::Ambiguous { matches } => anyhow::anyhow!(
                "--commit `{}` is ambiguous (matched {} reviewable commits for plan \
                 `{}`): {}. Pass a longer prefix or the full SHA.",
                args.commit,
                matches.len(),
                plan.as_str(),
                format_short_list(&matches),
            ),
        })?;

    let wire_path = feedback_path_wire(
        &author,
        &FeedbackTarget::Plan(plan.clone()),
        &target_sha,
        &reviewable,
    );
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

fn read_body(spec: &str) -> std::io::Result<String> {
    if spec == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        Ok(s)
    } else {
        std::fs::read_to_string(spec)
    }
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
