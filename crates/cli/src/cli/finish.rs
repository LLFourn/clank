//! `clank finish` — finalize an approved plan.
//!
//! Fully local: folds the repo with `rebuild::rebuild_repo`, builds
//! a typed `FinishPreviewResponse` via `crate::preview`, dispatches
//! on `readiness`, re-reads each sealed approval (verifying its
//! body hash against the local projection), then makes a single
//! `[<stem>] finish` commit. No daemon required.

use std::path::Path;

use super::{FinishArgs, repo_basename, resolve_repo};
use clank_core::api::{FinalizeBlockReason, FinalizeReadiness, FinishPreviewResponse};

pub async fn run(args: FinishArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;
    let policy = if args.no_cache {
        crate::rebuild::CachePolicy::Bypass
    } else {
        crate::rebuild::CachePolicy::Use
    };
    let state = crate::rebuild::rebuild_repo_with_policy(&repo, policy)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
    let plan_key = crate::cli::plan_resolve::resolve_plan(&state, &basename, args.plan.as_deref())?;
    let stem = plan_key.as_str().to_string();

    let preview = crate::preview::build_finish_preview(&repo, &state, &plan_key)
        .await
        .map_err(|e| anyhow::anyhow!("finish preview failed: {e}"))?;

    // Amend on an already-finished plan: rewrite HEAD's commit
    // (e.g. to refresh a stale message). The finalize tree is
    // already on disk, so skip the file-moving `finalize()` path.
    if args.amend && matches!(preview.readiness, FinalizeReadiness::AlreadyFinished) {
        require_head_is_finalize(&repo, &stem)?;
        if args.dry && (args.purge || args.squash.is_some()) {
            return dry_run_finish_composite(&stem, &preview, &args);
        }
        amend_already_finished(&repo, &stem, args.message.as_deref())?;
        println!("amended HEAD with finalize tree for `{stem}`");
        if args.purge || args.squash.is_some() {
            run_post_finalize_rewrite(&repo, &plan_key, args).await?;
        }
        return Ok(());
    }

    // Non-amend `--squash`/`--purge` on an already-finished plan:
    // the finalize tree is already on disk, so skip `finalize()`
    // (it would no-op) and run the rewrite directly. This un-blocks
    // a keep-`.clank/` squash decided AFTER the finish landed
    // (`finish-squash-idempotent-on-finished`). NOT factored with
    // the `--amend` branch above: the load-bearing difference is
    // that branch's `amend_already_finished` HEAD re-commit, which
    // must stay amend-only — this path only rewrites the range, it
    // does not touch the finalize commit. Plain `finish` (no
    // purge/squash) still falls through to the no-op below.
    if matches!(preview.readiness, FinalizeReadiness::AlreadyFinished)
        && (args.purge || args.squash.is_some())
    {
        if args.dry {
            return dry_run_finish_composite(&stem, &preview, &args);
        }
        run_post_finalize_rewrite(&repo, &plan_key, args).await?;
        return Ok(());
    }

    if !dispatch_readiness(&preview)? {
        return Ok(());
    }

    if args.amend {
        require_head_is_finalize(&repo, &stem)?;
    }

    if args.dry && (args.purge || args.squash.is_some()) {
        return dry_run_finish_composite(&stem, &preview, &args);
    }

    finalize(&repo, &stem, &preview, args.amend, args.message.as_deref()).await?;

    if args.amend {
        println!("amended HEAD with finalize tree for `{stem}`");
    } else {
        println!("finalized `{stem}`");
    }

    if args.purge || args.squash.is_some() {
        run_post_finalize_rewrite(&repo, &plan_key, args).await?;
    }
    Ok(())
}

/// Dry-run preview for `finish --purge`/`--squash`. The finalize
/// commit hasn't been created yet, so we emit a description of
/// the planned action without calling `finalize()` or running the
/// rewrite engine. Pipe-to-git isn't possible here because the
/// finalize commit doesn't exist — operator must run the live
/// command to materialize it before any rebase.
fn dry_run_finish_composite(
    stem: &str,
    preview: &FinishPreviewResponse,
    args: &FinishArgs,
) -> anyhow::Result<()> {
    println!("# clank finish --dry preview");
    println!("# plan: {}", preview.plan_id);
    let already_finished = matches!(preview.readiness, FinalizeReadiness::AlreadyFinished);
    let msg = args
        .message
        .as_deref()
        .map(str::to_string)
        .unwrap_or_else(|| format!("[{stem}] finish"));
    if already_finished && args.amend {
        // --amend re-commits the existing finalize.
        println!("# would amend HEAD finalize commit:");
        println!("#   message: {msg}");
    } else if already_finished {
        // Non-amend rewrite on an already-finished plan: the
        // finalize commit already exists and is NOT touched — only
        // the range is rewritten below. (Was previously mislabeled
        // "would create finalize commit"; codex c0c34ef.)
        println!(
            "# plan already finished; finalize commit left as-is (only the range is rewritten)."
        );
    } else {
        // Ready, not yet finalized: this run creates the finalize.
        println!("# would create finalize commit:");
        println!("#   message: {msg}");
        let approvers: Vec<&str> = preview
            .sealed_approvals
            .iter()
            .map(|a| a.author.as_str())
            .collect();
        println!("#   sealed approvals: {}", approvers.join(", "));
    }
    println!("#");
    if args.purge && args.squash.is_some() {
        println!(
            "# would then squash plan history into one commit and strip the finalize snapshot."
        );
        println!(
            "#   squash message: {}",
            args.squash.as_deref().unwrap_or("")
        );
    } else if args.squash.is_some() {
        println!(
            "# would then squash plan-attributed commits into one (finalize snapshot preserved)."
        );
        println!(
            "#   squash message: {}",
            args.squash.as_deref().unwrap_or("")
        );
    } else if args.purge {
        println!("# would then strip the plan's `.clank/` artifacts from history.");
    }
    println!("# (--dry: no commits, no refs updated)");
    Ok(())
}

async fn run_post_finalize_rewrite(
    repo: &std::path::Path,
    plan_key: &crate::lifecycle::PlanKey,
    args: FinishArgs,
) -> anyhow::Result<()> {
    // `--purge` semantics: strip the plan's `.clank/` paths from
    // history (including the just-landed finalize snapshot). Use
    // include_finalize=true so the snapshot is in the strip set.
    //
    // `--squash` without `--purge`: collapse plan-revision commits
    // into one but PRESERVE the finalize snapshot. Use
    // include_finalize=false so the snapshot survives the squash.
    let include_finalize = args.purge;
    let policy = if args.no_cache {
        crate::rebuild::CachePolicy::Bypass
    } else {
        crate::rebuild::CachePolicy::Use
    };
    let state = crate::rebuild::rebuild_repo_with_policy(repo, policy)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
    let preview = crate::preview::build_rewrite_preview(repo, &state, plan_key, include_finalize)
        .await
        .map_err(|e| anyhow::anyhow!("rewrite preview failed: {e}"))?;
    let stem = plan_key.as_str();
    crate::cli::rewrite::run(crate::cli::rewrite::RewriteOpts {
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
    if let Some(branch) = args.into_branch.as_deref() {
        println!("rewritten history on branch `{branch}`");
    } else if args.squash.is_some() {
        println!("squashed `{stem}` in place");
    } else {
        println!("purged `{stem}` from history");
    }
    Ok(())
}

/// The CLI's one dispatch on the typed decision. Returns `true`
/// when the caller should continue with finalize, `false` for the
/// no-op "already finished" case (so the caller can return cleanly
/// without `std::process::exit` cutting tokio's shutdown short).
fn dispatch_readiness(preview: &FinishPreviewResponse) -> anyhow::Result<bool> {
    match &preview.readiness {
        FinalizeReadiness::Ready => Ok(true),
        FinalizeReadiness::AlreadyFinished => {
            println!("`{}` is already finished; nothing to do", preview.plan_id);
            Ok(false)
        }
        FinalizeReadiness::Blocked { reasons } => {
            let lines: Vec<String> = reasons.iter().map(reason_to_msg).collect();
            anyhow::bail!(
                "cannot finalize `{}`:\n  - {}",
                preview.plan_id,
                lines.join("\n  - "),
            )
        }
    }
}

fn reason_to_msg(reason: &FinalizeBlockReason) -> String {
    match reason {
        FinalizeBlockReason::NoReviewableCommit => {
            "no reviewable commit attributed to this plan yet".into()
        }
        FinalizeBlockReason::NotFinished { state } => match state {
            clank_core::vocab::CommitGateState::Approved => {
                "latest reviewable commit is approved but not FINISHED — \
                 a reviewer needs to mark FINISHED before finalize"
                    .into()
            }
            clank_core::vocab::CommitGateState::ChangesRequested => {
                "changes requested on the latest reviewable commit; address them \
                 and re-commit before finalize"
                    .into()
            }
            clank_core::vocab::CommitGateState::Unreviewed => {
                "latest reviewable commit hasn't been reviewed yet".into()
            }
            clank_core::vocab::CommitGateState::Finished => {
                // Logically unreachable — compute_finalize_readiness
                // only emits NotFinished when state != Finished.
                "gate is unexpectedly Finished but finalize is blocked".into()
            }
            clank_core::vocab::CommitGateState::Blocked => {
                "plan has an open block — clear the block before finalize".into()
            }
            clank_core::vocab::CommitGateState::ApprovedPendingGate => {
                "latest reviewable commit is approved by commit-tier reviewers; \
                 gate-tier reviewers haven't all weighed in yet — wait for their FINISHED \
                 before running finalize"
                    .into()
            }
        },
        FinalizeBlockReason::PlanFileMissing => "plan file is missing from the worktree".into(),
        FinalizeBlockReason::PlanFileDirty => {
            "plan file has uncommitted changes; commit or stash first".into()
        }
    }
}

async fn finalize(
    repo: &Path,
    stem: &str,
    _preview: &FinishPreviewResponse,
    amend: bool,
    message: Option<&str>,
) -> anyhow::Result<()> {
    let finished_dir = repo.join(".clank/finished");
    std::fs::create_dir_all(&finished_dir)?;

    // Remove legacy directory-style finished marker if present.
    let legacy_dir = finished_dir.join(stem);
    if legacy_dir.is_dir() {
        std::fs::remove_dir_all(&legacy_dir)?;
    }
    // Remove legacy no-extension marker file if present.
    let legacy_marker = finished_dir.join(stem);
    if legacy_marker.is_file() {
        std::fs::remove_file(&legacy_marker)?;
    }

    // Move the plan file into finished/ to commit as a rename/move.
    let plan_path = repo.join(format!(".clank/plans/{stem}.md"));
    let finished_path = finished_dir.join(format!("{stem}.md"));
    if plan_path.exists() {
        std::fs::copy(&plan_path, &finished_path)?;
    } else {
        // Plan file missing (e.g. hidden); write an empty finished marker.
        std::fs::write(&finished_path, "")?;
    }

    let rel_plan = format!(".clank/plans/{stem}.md");
    let rel_finished = format!(".clank/finished/{stem}.md");
    git_run(repo, &["rm", "--quiet", "--force", "--", &rel_plan])?;
    git_run(repo, &["add", "--", &rel_finished])?;

    let default_msg = format!("[{stem}] finish");
    let msg = message.unwrap_or(&default_msg);
    let mut commit_args: Vec<&str> = vec!["commit", "--quiet", "-m", msg];
    if amend {
        commit_args.push("--amend");
    }
    git_run(repo, &commit_args)?;
    Ok(())
}

fn amend_already_finished(repo: &Path, stem: &str, message: Option<&str>) -> anyhow::Result<()> {
    let default_msg = format!("[{stem}] finish");
    let msg = message.unwrap_or(&default_msg);
    git_run(repo, &["commit", "--amend", "--quiet", "-m", msg])
}

fn require_head_is_finalize(repo: &Path, stem: &str) -> anyhow::Result<()> {
    if !head_is_finalize_for(repo, stem)? {
        anyhow::bail!(
            "--amend requires HEAD to be a finalize commit for plan `{stem}`; \
             run `clank finish` without --amend instead",
        );
    }
    Ok(())
}

fn head_is_finalize_for(repo: &Path, stem: &str) -> anyhow::Result<bool> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"])
        .output()?;
    if !output.status.success() {
        return Ok(false);
    }
    let finished_path = format!(".clank/finished/{stem}.md");
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|l| l == finished_path))
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

#[cfg(test)]
mod tests {
    use super::*;

    use clank_core::api::{FinalizeReadiness, FinishPreviewResponse};
    use clank_core::vocab::{CommitGateState, PlanWorktreeStatus};

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "--quiet", "--initial-branch=main"]);
        run_git(dir.path(), &["config", "user.email", "test@test"]);
        run_git(dir.path(), &["config", "user.name", "test"]);
        run_git(dir.path(), &["config", "commit.gpgsign", "false"]);
        dir
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn write_at(repo: &Path, rel: &str, body: &str) {
        let p = repo.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn mk_preview_ready() -> FinishPreviewResponse {
        FinishPreviewResponse {
            plan_id: "clank/foo.md".into(),
            plan_path: ".clank/plans/foo.md".into(),
            readiness: FinalizeReadiness::Ready,
            gate_state: CommitGateState::Approved,
            latest_reviewable_sha: None,
            plan_worktree_status: PlanWorktreeStatus::Clean,
            is_finished: false,
            sealed_approvals: vec![],
        }
    }

    #[tokio::test]
    async fn finalize_writes_empty_marker_and_commits() {
        let dir = init_repo();
        // A plan file must exist so finalize can move it.
        write_at(dir.path(), ".clank/plans/foo.md", "# foo\n");
        run_git(dir.path(), &["add", "-A"]);
        run_git(dir.path(), &["commit", "--quiet", "-m", "seed"]);

        let preview = mk_preview_ready();
        finalize(dir.path(), "foo", &preview, false, None)
            .await
            .unwrap();

        let finished = dir.path().join(".clank/finished/foo.md");
        assert!(finished.exists(), "finished file should exist");
        let plan = dir.path().join(".clank/plans/foo.md");
        assert!(!plan.exists(), "plan file should be removed");
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap().trim(),
            "[foo] finish"
        );
    }

    #[tokio::test]
    async fn amend_already_finished_rewrites_message_without_touching_finished_file() {
        let dir = init_repo();
        write_at(dir.path(), ".clank/plans/foo.md", "# foo body\n");
        run_git(dir.path(), &["add", "-A"]);
        run_git(dir.path(), &["commit", "--quiet", "-m", "seed"]);

        let preview = mk_preview_ready();
        finalize(dir.path(), "foo", &preview, false, None)
            .await
            .unwrap();

        let finished = dir.path().join(".clank/finished/foo.md");
        let original_body = std::fs::read_to_string(&finished).unwrap();
        assert_eq!(original_body, "# foo body\n");

        amend_already_finished(dir.path(), "foo", Some("custom amend message")).unwrap();

        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap().trim(),
            "custom amend message"
        );

        assert_eq!(std::fs::read_to_string(&finished).unwrap(), original_body);
    }

    #[tokio::test]
    async fn finalize_replaces_old_directory_style_marker() {
        let dir = init_repo();
        write_at(dir.path(), ".clank/plans/foo.md", "# foo\n");
        run_git(dir.path(), &["add", "-A"]);
        run_git(dir.path(), &["commit", "--quiet", "-m", "seed"]);

        // Simulate a pre-existing legacy directory-style marker.
        write_at(dir.path(), ".clank/finished/foo/codex.md", "APPROVE\n");

        let preview = mk_preview_ready();
        finalize(dir.path(), "foo", &preview, false, None)
            .await
            .unwrap();

        let finished = dir.path().join(".clank/finished/foo.md");
        assert!(finished.is_file(), "should produce .md file");
        let legacy_dir = dir.path().join(".clank/finished/foo");
        assert!(!legacy_dir.is_dir(), "legacy directory should be removed");
    }
}
