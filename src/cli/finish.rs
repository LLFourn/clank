//! `trinity finish` — finalize an approved plan.
//!
//! Fully local: folds the repo with `rebuild::rebuild_repo`, builds
//! a typed `FinishPreviewResponse` via `crate::preview`, dispatches
//! on `readiness`, re-reads each sealed approval (verifying its
//! body hash against the local projection), then makes a single
//! `Finalize <stem>` commit. No daemon required.

use std::path::Path;

use super::{FinishArgs, repo_basename, resolve_repo};
use trinity_core::api::{FinalizeBlockReason, FinalizeReadiness, FinishPreviewResponse};

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
    if !dispatch_readiness(&preview)? {
        return Ok(());
    }

    if args.amend && !head_is_finalize_for(&repo, &stem)? {
        anyhow::bail!(
            "--amend requires HEAD to be a finalize commit for plan `{stem}`; \
             run `trinity finish` without --amend instead",
        );
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
    println!("# trinity finish --dry preview");
    println!("# plan: {}", preview.plan_id);
    println!("# would create finalize commit:");
    let msg = args
        .message
        .as_deref()
        .map(str::to_string)
        .unwrap_or_else(|| format!("Finalize {stem}"));
    println!("#   message: {msg}");
    let approvers: Vec<&str> = preview
        .sealed_approvals
        .iter()
        .map(|a| a.author.as_str())
        .collect();
    println!("#   sealed approvals: {}", approvers.join(", "));
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
        println!("# would then strip the plan's `.trinity/` artifacts from history.");
    }
    println!("# (--dry: no commits, no refs updated)");
    Ok(())
}

async fn run_post_finalize_rewrite(
    repo: &std::path::Path,
    plan_key: &crate::lifecycle::PlanKey,
    args: FinishArgs,
) -> anyhow::Result<()> {
    // `--purge` semantics: strip the plan's `.trinity/` paths from
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
        FinalizeBlockReason::GateNotApproved { state } => {
            format!("gate is {} (need approved)", state.as_str())
        }
        FinalizeBlockReason::PlanFileMissing => "plan file is missing from the worktree".into(),
        FinalizeBlockReason::PlanFileDirty => {
            "plan file has uncommitted changes; commit or stash first".into()
        }
    }
}

/// Read+hash every approval into memory BEFORE touching
/// `.trinity/finished/<stem>/`. If any read fails or any hash
/// drifts, we abort cleanly and the operator's existing finalize
/// snapshot is untouched. Especially important for `--amend`: a
/// failure must not strand the operator between snapshots.
async fn finalize(
    repo: &Path,
    stem: &str,
    preview: &FinishPreviewResponse,
    amend: bool,
    message: Option<&str>,
) -> anyhow::Result<()> {
    if preview.sealed_approvals.is_empty() {
        anyhow::bail!(
            "readiness=Ready but no sealed approvals to write — refusing to seal an empty set"
        );
    }

    let mut verified: Vec<(String, String)> = Vec::with_capacity(preview.sealed_approvals.len());
    for approval in &preview.sealed_approvals {
        let source = repo.join(&approval.source_path);
        let body = std::fs::read_to_string(&source).map_err(|e| {
            anyhow::anyhow!(
                "failed to read sealed-approval source `{}`: {e}",
                approval.source_path,
            )
        })?;
        let observed = crate::lifecycle::content_hash(&body);
        if observed != approval.body_hash {
            anyhow::bail!(
                "sealed approval for `{}` drifted between preview and commit \
                 (daemon hash {}, on-disk hash {}); re-run after the watcher \
                 catches up, or revert the file",
                approval.author.as_str(),
                approval.body_hash.as_str(),
                observed.as_str(),
            );
        }
        verified.push((format!("{}.md", approval.author.as_str()), body));
    }

    let finished_dir = repo.join(".trinity/finished").join(stem);
    if finished_dir.exists() {
        std::fs::remove_dir_all(&finished_dir)?;
    }
    std::fs::create_dir_all(&finished_dir)?;
    for (name, body) in &verified {
        std::fs::write(finished_dir.join(name), body)?;
    }

    // `-A` so that approver files removed from the new set are
    // staged as deletions. Plain `git add <dir>` only stages
    // additions/modifications; an --amend would otherwise inherit
    // the prior commit's approver list and silently keep a stale
    // file when the new approver set is a strict subset.
    let rel_finished = format!(".trinity/finished/{stem}");
    git_run(repo, &["add", "-A", "--", &rel_finished])?;

    let default_msg = format!("Finalize {stem}");
    let msg = message.unwrap_or(&default_msg);
    let mut commit_args: Vec<&str> = vec!["commit", "--quiet", "-m", msg];
    if amend {
        commit_args.push("--amend");
    }
    git_run(repo, &commit_args)?;
    Ok(())
}

/// True iff HEAD is a finalize commit for this plan — defined as
/// "every file changed by HEAD lives under `.trinity/finished/<stem>/`."
/// Used by `--amend` to refuse amending an unrelated commit.
fn head_is_finalize_for(repo: &Path, stem: &str) -> anyhow::Result<bool> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"])
        .output()?;
    if !output.status.success() {
        return Ok(false);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let prefix = format!(".trinity/finished/{stem}/");
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    Ok(!lines.is_empty() && lines.iter().all(|l| l.starts_with(&prefix)))
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

    // Plan-arg parsing tests live in `crate::cli::plan_resolve`.

    use trinity_core::api::{FinalizeReadiness, FinishPreviewResponse, SealedApproval};
    use trinity_core::ids::AgentLabel;
    use trinity_core::vocab::{CommitGateState, PlanWorktreeStatus};

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

    fn mk_preview_ready(approval_path: &str, body: &str) -> FinishPreviewResponse {
        FinishPreviewResponse {
            plan_id: "trinity/foo.md".into(),
            plan_path: ".trinity/plans/foo.md".into(),
            readiness: FinalizeReadiness::Ready,
            gate_state: CommitGateState::Approved,
            latest_reviewable_sha: None,
            plan_worktree_status: PlanWorktreeStatus::Clean,
            is_finished: false,
            sealed_approvals: vec![SealedApproval {
                author: AgentLabel::parse("codex").unwrap(),
                source_path: approval_path.to_string(),
                body_hash: crate::lifecycle::content_hash(body),
            }],
        }
    }

    #[tokio::test]
    async fn finalize_writes_approver_file_and_commits() {
        let dir = init_repo();
        write_at(dir.path(), "README.md", "seed\n");
        run_git(dir.path(), &["add", "-A"]);
        run_git(dir.path(), &["commit", "--quiet", "-m", "seed"]);

        let approval = ".trinity/feedback/foo/abcdef/codex.md";
        let body = "APPROVE\n\nlgtm\n";
        write_at(dir.path(), approval, body);

        let preview = mk_preview_ready(approval, body);
        finalize(dir.path(), "foo", &preview, false, None)
            .await
            .unwrap();

        let dest = dir.path().join(".trinity/finished/foo/codex.md");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), body);
        // A finalize commit landed.
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap().trim(),
            "Finalize foo"
        );
    }

    #[tokio::test]
    async fn finalize_aborts_on_hash_drift_without_touching_existing_snapshot() {
        let dir = init_repo();
        write_at(dir.path(), "README.md", "seed\n");
        run_git(dir.path(), &["add", "-A"]);
        run_git(dir.path(), &["commit", "--quiet", "-m", "seed"]);

        // Pre-existing finalize snapshot (e.g. from a prior run).
        let stale_snapshot = ".trinity/finished/foo/codex.md";
        write_at(dir.path(), stale_snapshot, "OLD APPROVE\n");

        let approval = ".trinity/feedback/foo/abcdef/codex.md";
        write_at(dir.path(), approval, "APPROVE\n\non-disk body\n");
        // Preview expects a different body — the daemon's projection
        // is stale relative to disk.
        let preview = mk_preview_ready(approval, "APPROVE\n\nDAEMON BELIEVED THIS\n");

        let err = finalize(dir.path(), "foo", &preview, false, None)
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("drifted"), "got: {msg}");

        // The pre-existing snapshot is intact — abort happened BEFORE
        // mutation.
        assert_eq!(
            std::fs::read_to_string(dir.path().join(stale_snapshot)).unwrap(),
            "OLD APPROVE\n",
            "drift abort must not touch the existing finalize snapshot",
        );
    }

    #[tokio::test]
    async fn finalize_aborts_on_missing_source_without_touching_existing_snapshot() {
        let dir = init_repo();
        write_at(dir.path(), "README.md", "seed\n");
        run_git(dir.path(), &["add", "-A"]);
        run_git(dir.path(), &["commit", "--quiet", "-m", "seed"]);

        let stale_snapshot = ".trinity/finished/foo/codex.md";
        write_at(dir.path(), stale_snapshot, "OLD APPROVE\n");

        // Approval file does NOT exist on disk.
        let preview = mk_preview_ready(".trinity/feedback/foo/abcdef/codex.md", "APPROVE\n");

        let err = finalize(dir.path(), "foo", &preview, false, None)
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("failed to read"), "got: {msg}");

        assert_eq!(
            std::fs::read_to_string(dir.path().join(stale_snapshot)).unwrap(),
            "OLD APPROVE\n",
            "missing-source abort must not touch the existing finalize snapshot",
        );
    }

    #[tokio::test]
    async fn finalize_amend_removes_approver_no_longer_in_set() {
        let dir = init_repo();
        write_at(dir.path(), "README.md", "seed\n");
        run_git(dir.path(), &["add", "-A"]);
        run_git(dir.path(), &["commit", "--quiet", "-m", "seed"]);

        // First finalize: two approvers.
        let codex_approval = ".trinity/feedback/foo/abcdef/codex.md";
        let claude_approval = ".trinity/feedback/foo/abcdef/claude.md";
        write_at(dir.path(), codex_approval, "APPROVE codex\n");
        write_at(dir.path(), claude_approval, "APPROVE claude\n");

        let first = FinishPreviewResponse {
            plan_id: "trinity/foo.md".into(),
            plan_path: ".trinity/plans/foo.md".into(),
            readiness: FinalizeReadiness::Ready,
            gate_state: CommitGateState::Approved,
            latest_reviewable_sha: None,
            plan_worktree_status: PlanWorktreeStatus::Clean,
            is_finished: false,
            sealed_approvals: vec![
                SealedApproval {
                    author: AgentLabel::parse("codex").unwrap(),
                    source_path: codex_approval.into(),
                    body_hash: crate::lifecycle::content_hash("APPROVE codex\n"),
                },
                SealedApproval {
                    author: AgentLabel::parse("claude").unwrap(),
                    source_path: claude_approval.into(),
                    body_hash: crate::lifecycle::content_hash("APPROVE claude\n"),
                },
            ],
        };
        finalize(dir.path(), "foo", &first, false, None)
            .await
            .unwrap();

        // Sanity: both files in HEAD's tree.
        let ls = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args([
                "ls-tree",
                "-r",
                "--name-only",
                "HEAD",
                ".trinity/finished/foo/",
            ])
            .output()
            .unwrap();
        let listed = String::from_utf8(ls.stdout).unwrap();
        assert!(listed.contains("codex.md"));
        assert!(listed.contains("claude.md"));

        // Amend with a strict subset (claude only). The codex approver
        // file must be staged as a deletion and disappear from HEAD's
        // tree — otherwise `git add <dir>` would leave the stale file
        // around.
        let second = FinishPreviewResponse {
            sealed_approvals: vec![SealedApproval {
                author: AgentLabel::parse("claude").unwrap(),
                source_path: claude_approval.into(),
                body_hash: crate::lifecycle::content_hash("APPROVE claude\n"),
            }],
            ..first
        };
        finalize(dir.path(), "foo", &second, true, None)
            .await
            .unwrap();

        let ls = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args([
                "ls-tree",
                "-r",
                "--name-only",
                "HEAD",
                ".trinity/finished/foo/",
            ])
            .output()
            .unwrap();
        let listed = String::from_utf8(ls.stdout).unwrap();
        assert!(
            !listed.contains("codex.md"),
            "codex approver should be removed after amend; got:\n{listed}"
        );
        assert!(listed.contains("claude.md"));
    }
}
