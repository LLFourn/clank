//! `trinity finish` — finalize an approved plan.
//!
//! The CLI never re-derives projection state. It calls the daemon's
//! `finish_preview` endpoint, dispatches on the typed `readiness`
//! field, re-reads each sealed approval (verifying the body hash
//! matches the daemon's projection), then makes a single
//! `Finalize <stem>` commit.

use std::path::Path;

use super::{FinishArgs, repo_basename, resolve_repo};
use trinity_core::api::{FinalizeBlockReason, FinalizeReadiness, FinishPreviewResponse};

pub async fn run(args: FinishArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;
    let daemon = args.daemon.trim_end_matches('/').to_string();
    let stem = resolve_stem_or_infer(&args.plan, &basename, &daemon).await?;

    let preview = fetch_preview(&daemon, &basename, &stem).await?;
    dispatch_readiness(&preview)?;

    if args.amend && !head_is_finalize_for(&repo, &stem)? {
        anyhow::bail!(
            "--amend requires HEAD to be a finalize commit for plan `{stem}`; \
             run `trinity finish` without --amend instead",
        );
    }

    finalize(&repo, &stem, &preview, args.amend, args.message.as_deref()).await?;

    if args.amend {
        println!("amended HEAD with finalize tree for `{stem}`");
    } else {
        println!("finalized `{stem}`");
    }
    Ok(())
}

/// Public for `cli::purge` (same parsing rules across both
/// subcommands). Falls back to single-active-plan inference when
/// the operator passed no plan arg.
pub(crate) async fn resolve_stem_or_infer_for_purge(
    plan: &Option<String>,
    expected_basename: &str,
    daemon: &str,
) -> anyhow::Result<String> {
    resolve_stem_or_infer(plan, expected_basename, daemon).await
}

/// Plan-arg resolution with inference. If `plan` is `Some`, parse
/// it. If `None`, ask the daemon for the repo's active in-flight
/// plans and require exactly one.
async fn resolve_stem_or_infer(
    plan: &Option<String>,
    expected_basename: &str,
    daemon: &str,
) -> anyhow::Result<String> {
    if plan.is_some() {
        return resolve_stem(plan, expected_basename);
    }
    infer_single_active_plan(daemon, expected_basename).await
}

async fn infer_single_active_plan(daemon: &str, basename: &str) -> anyhow::Result<String> {
    let url = format!("{daemon}/api/plans?repo={basename}");
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
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
    let response = resp.json::<trinity_core::api::ListPlansResponse>().await?;
    let active: Vec<&trinity_core::api::PlanRow> = response
        .plans
        .iter()
        .filter(|row| row.lifecycle == trinity_core::vocab::PlanLifecycle::Active)
        .filter(|row| {
            row.plan_id
                .as_deref()
                .and_then(|pid| pid.split_once('/'))
                .map(|(b, _)| b == basename)
                .unwrap_or(false)
        })
        .collect();
    match active.as_slice() {
        [one] => Ok(one.slug.clone()),
        [] => anyhow::bail!(
            "no active in-flight plan to infer; pass a plan id explicitly. \
             repo: `{basename}`",
        ),
        many => {
            let candidates: Vec<&str> = many.iter().filter_map(|p| p.plan_id.as_deref()).collect();
            anyhow::bail!(
                "ambiguous: {} active plans in `{basename}`; pass one explicitly. \
                 candidates: {}",
                many.len(),
                candidates.join(", "),
            )
        }
    }
}

/// Parse the `<plan>` CLI argument into a plan stem the wire form
/// expects. Accepts `<basename>/<stem>.md` (full plan id; must
/// address the repo `--repo`/cwd resolves to), `<stem>.md`, or
/// `<stem>`. The stem returned never includes the `.md` extension;
/// the caller re-adds it when building wire paths.
fn resolve_stem(plan: &Option<String>, expected_basename: &str) -> anyhow::Result<String> {
    let Some(raw) = plan else {
        anyhow::bail!("plan argument required");
    };
    let raw = raw.trim();
    if raw.is_empty() {
        anyhow::bail!("plan argument is empty");
    }
    if let Some((basename, rest)) = raw.split_once('/') {
        if basename != expected_basename {
            anyhow::bail!(
                "plan id `{raw}` names repo `{basename}` but we're operating on `{expected_basename}` \
                 — pass `--repo` to override, or invoke from inside the right repo",
            );
        }
        return Ok(rest.trim_end_matches(".md").to_string());
    }
    Ok(raw.trim_end_matches(".md").to_string())
}

async fn fetch_preview(
    daemon: &str,
    basename: &str,
    stem: &str,
) -> anyhow::Result<FinishPreviewResponse> {
    let url = format!("{daemon}/api/plan/{basename}/{stem}.md/finish_preview");
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
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
    Ok(resp.json::<FinishPreviewResponse>().await?)
}

/// The CLI's one dispatch on the daemon's typed decision. Anything
/// past this point assumes `readiness == Ready`.
fn dispatch_readiness(preview: &FinishPreviewResponse) -> anyhow::Result<()> {
    match &preview.readiness {
        FinalizeReadiness::Ready => Ok(()),
        FinalizeReadiness::AlreadyFinished => {
            println!("`{}` is already finished; nothing to do", preview.plan_id);
            std::process::exit(0);
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
            "daemon returned readiness=Ready but no sealed approvals — refusing to seal an empty set"
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

    let rel_finished = format!(".trinity/finished/{stem}");
    git_run(repo, &["add", &rel_finished])?;

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

    #[test]
    fn resolve_stem_accepts_bare_stem() {
        assert_eq!(resolve_stem(&Some("foo".into()), "trinity").unwrap(), "foo");
    }

    #[test]
    fn resolve_stem_strips_dot_md() {
        assert_eq!(
            resolve_stem(&Some("foo.md".into()), "trinity").unwrap(),
            "foo"
        );
    }

    #[test]
    fn resolve_stem_accepts_full_id_when_basename_matches() {
        assert_eq!(
            resolve_stem(&Some("trinity/foo.md".into()), "trinity").unwrap(),
            "foo"
        );
    }

    #[test]
    fn resolve_stem_rejects_full_id_with_wrong_basename() {
        let err = resolve_stem(&Some("other/foo.md".into()), "trinity").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("other"), "unexpected error: {msg}");
        assert!(msg.contains("trinity"), "unexpected error: {msg}");
    }

    #[test]
    fn resolve_stem_rejects_empty_arg() {
        assert!(resolve_stem(&Some("   ".into()), "trinity").is_err());
        assert!(resolve_stem(&Some("".into()), "trinity").is_err());
    }

    #[test]
    fn resolve_stem_rejects_missing_arg_at_parse_layer() {
        // resolve_stem itself bails on None; the inference fallback
        // lives one layer up in `resolve_stem_or_infer`.
        let err = resolve_stem(&None, "trinity").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("plan argument required"), "got: {msg}");
    }

    // ---- finalize() tests (direct, no HTTP) ----

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
}
