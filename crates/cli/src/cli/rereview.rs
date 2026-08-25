//! `clank rereview [plan]` — open a fresh review round on a plan's
//! latest reviewable commit.
//!
//! Promotion can manufacture a self-review demand, and clank resolves
//! that by forcing a CONTINUE from the demoted agent. That verdict is
//! a roster artefact, not a judgment — so the new master needs a way
//! to ask for a real one. This is it: the commit is rewritten with the
//! same tree and the same message, and the WHOLE current roster owes
//! it a review again.
//!
//! The target's own feedback is deliberately NOT migrated onto the new
//! sha (every descendant's is). That is the whole mechanism: the
//! rewritten commit carries nothing, so every reviewer — including the
//! demoted agent, now legitimately — has a pending review. The old
//! sha's feedback stays behind, inert.

use super::{RereviewArgs, resolve_repo};
use clank_core::ids::CommitSha;

/// Rewrite `plan`'s latest reviewable commit into a fresh one.
///
/// Returns the `(old, new)` pair for the target.
pub async fn rereview_plan(
    repo: &std::path::Path,
    plan: Option<&str>,
) -> anyhow::Result<(CommitSha, CommitSha)> {
    let basename = super::repo_basename(repo)?;
    let state = crate::rebuild::rebuild_repo_with_policy(repo, crate::rebuild::CachePolicy::Bypass)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
    let plan_key = crate::cli::plan_resolve::resolve_plan(&state, &basename, plan)?;

    let target = state
        .fold
        .plans
        .get(&plan_key)
        .and_then(|ps| {
            ps.commits
                .iter()
                .rev()
                .find(|e| e.touched_plan || e.touched_code)
                .map(|e| e.sha.clone())
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "`{}` has no reviewable commit to re-open — nothing has been \
                 committed for it yet",
                plan_key.as_str()
            )
        })?;

    let head =
        crate::git_io::rev_parse_head(repo)?.ok_or_else(|| anyhow::anyhow!("repo has no HEAD"))?;
    let message = crate::git_io::commit_message_raw_at(repo, &target)?;

    let pairs = crate::cli::rewrite::reword_in_place(crate::cli::rewrite::RewordOpts {
        repo,
        target_sha: &target,
        head_sha: &head,
        new_message: &message,
        // Plans live on the default branch, and so does the promotion
        // handoff this exists to unstick. Inheriting the engine's
        // protected-branch refusal would make the command unusable
        // exactly where it is needed.
        allow_rewrite_protected: true,
        // The point of the command: mint a NEW review target even
        // though nothing but the commit's identity changes.
        distinct_target: true,
        dry: false,
    })
    .await?;

    let (old, new) = pairs
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("rewrite reported no commits"))?;
    // Defensive: `distinct_target` guarantees this by construction.
    // If it ever fires, the object model changed under us — and
    // silently doing nothing would be the worse answer, since the
    // caller believes a review round just opened.
    if old == new {
        anyhow::bail!(
            "rewriting {} produced the same commit, so no new review round opened",
            &old.as_str()[..7.min(old.as_str().len())]
        );
    }

    // Descendants keep their feedback; the TARGET deliberately loses
    // its own. That absence is what re-opens the round.
    crate::cli::rewire::migrate_feedback_pairs(repo, &pairs[1..])?;
    Ok((old, new))
}

pub async fn run(args: RereviewArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let (old, new) = rereview_plan(&repo, args.plan.as_deref()).await?;
    let short = |s: &CommitSha| s.as_str()[..7.min(s.as_str().len())].to_string();
    println!(
        "re-opened review: {} → {} (every reviewer owes a fresh verdict)",
        short(&old),
        short(&new)
    );
    Ok(())
}
