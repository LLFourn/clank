//! `clank finish` — finalize an continued plan.
//!
//! Fully local: folds the repo with `rebuild::rebuild_repo`, builds
//! a typed `FinishPreviewResponse` via `crate::preview`, dispatches
//! on `readiness`, re-reads each sealed approval (verifying its
//! body hash against the local projection), then makes a single
//! `[<stem>] finish` commit. No daemon required.

use std::path::Path;

use super::{FinishArgs, repo_basename, resolve_repo};
use clank_core::api::{FinalizeBlockReason, FinalizeReadiness, FinishPreviewResponse};

pub async fn run(mut args: FinishArgs) -> anyhow::Result<()> {
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

    // Repeated `-m` values compose git-style: subject, blank line, body…
    let message = compose_finish_message(&args.message);

    // `finish.autosquash`: treat a plain finish as `--squash <the -m message>`
    // — collapse the plan into one commit carrying the whole-plan message.
    // Only fills when `--squash` is unset (explicit wins), `--purge` is absent
    // (respect its explicit rewrite), and `--no-squash` isn't given (per-finish
    // opt-out). `-m` stays MANDATORY: an empty `-m` composes to None, so
    // `args.squash` stays None → the normal path → validation rejects.
    let cfg = crate::cli::config::load(&repo);
    if cfg.finish.autosquash && args.squash.is_none() && !args.purge && !args.no_squash {
        args.squash = message.clone();
    }

    // Validate the message that will BECOME the final finish commit's message.
    // With `--squash` that's the squash MSG (the finalize/amend commit made
    // first is collapsed away by `apply_squash`), so validating `-m` there
    // would miss the real message. `--purge` is NOT exempt: it authors a
    // finalize commit that survives a refused strip rewrite (see
    // `message_requiring_validation`).
    let already_finished = matches!(preview.readiness, FinalizeReadiness::AlreadyFinished);
    if let Some(final_msg) = message_requiring_validation(
        already_finished,
        args.amend,
        message.as_deref(),
        args.squash.as_deref(),
    ) {
        validate_finish_message(final_msg, &stem)?;
    }

    // The transient finalize/amend commit carries the message that ultimately
    // LANDS — the squash MSG when squashing — so a post-finalize rewrite that
    // is refused (e.g. protected-branch) leaves the validated message on HEAD,
    // never the `[stem] finish` placeholder (codex 053f9d1). It also renames
    // the plan file, so its subject must carry the `[<stem>]` tag or it trips
    // `fix_commit_tag` — `ensure_plan_tag` adds it (idempotently) after
    // validation.
    let commit_message = finalize_commit_message(args.squash.as_deref(), message.as_deref())
        .map(|m| ensure_plan_tag(m, &stem));

    // Amend on an already-finished plan: rewrite HEAD's commit
    // (e.g. to refresh a stale message). The finalize tree is
    // already on disk, so skip the file-moving `finalize()` path.
    if args.amend && already_finished {
        require_head_is_finalize(&repo, &stem)?;
        if args.dry && (args.purge || args.squash.is_some()) {
            return dry_run_finish_composite(&stem, &preview, &args);
        }
        amend_already_finished(&repo, &stem, commit_message.as_deref())?;
        println!("amended HEAD with finalize tree for `{stem}`");
        if args.purge || args.squash.is_some() {
            run_post_finalize_rewrite(&repo, &plan_key, args).await?;
        }
        return Ok(());
    }

    // A bare `-m` on an already-finished plan (no `--amend`, no purge/squash)
    // rewrites the finalize commit's message — the ergonomic way to fix or
    // improve the whole-plan summary after the finish landed, without the
    // `--amend` ceremony.
    if already_finished && message.is_some() && !args.amend && !args.purge && args.squash.is_none()
    {
        require_head_is_finalize(&repo, &stem)?;
        if args.dry {
            println!("# clank finish --dry: would rewrite the finish message for `{stem}`");
            return Ok(());
        }
        amend_already_finished(&repo, &stem, commit_message.as_deref())?;
        println!("rewrote finish message for `{stem}`");
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
    if already_finished && (args.purge || args.squash.is_some()) {
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

    finalize(
        &repo,
        &stem,
        &preview,
        args.amend,
        commit_message.as_deref(),
    )
    .await?;

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

/// Join repeated `-m` values into one message, git-style: each becomes a
/// paragraph separated by a blank line. `None` when no `-m` was given.
fn compose_finish_message(parts: &[String]) -> Option<String> {
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// The message that will BECOME the final finish commit's message and so must
/// be validated — or `None` if this invocation authors no finish commit.
///
/// - `--squash` collapses the range into one commit carrying the squash MSG,
///   so THAT is validated (the transient finalize/amend commit is squashed
///   away by `apply_squash`).
/// - otherwise the finalize/amend message is validated on any authoring path:
///   finalize create, `--amend`, or a bare `-m` rewrite on a finished plan.
///   `--purge` is NOT exempt (codex 53a9edb): it still CREATES a finalize
///   commit first, and if the strip rewrite is refused (protected branch,
///   etc.) that commit SURVIVES — so it must not be the `[stem] finish`
///   placeholder either.
/// - a no-op finish on an already-finished plan (no `--amend`, no `-m`, and
///   any purge/squash handled directly by the retroactive path) lands
///   nothing.
///
/// The inner `Option<&str>` is the message itself — `Some(None)` means "a
/// message is required here but none was supplied", which validation rejects.
fn message_requiring_validation<'a>(
    already_finished: bool,
    amend: bool,
    message: Option<&'a str>,
    squash: Option<&'a str>,
) -> Option<Option<&'a str>> {
    if squash.is_some() {
        Some(squash)
    } else if !already_finished || amend || message.is_some() {
        Some(message)
    } else {
        None
    }
}

/// The message to stamp on the transient finalize/amend commit: the squash
/// MSG when squashing (so a refused squash rewrite leaves the validated
/// landing message, not the `[stem] finish` placeholder), else the `-m`.
fn finalize_commit_message<'a>(
    squash: Option<&'a str>,
    message: Option<&'a str>,
) -> Option<&'a str> {
    squash.or(message)
}

/// Ensure a finish/squash commit's SUBJECT carries the plan's `[<stem>]` tag.
/// The finalize commit renames `plans/<stem>.md` → `finished/<stem>.md` (and
/// the squash commit collapses that rename in), so it touches the plan file
/// and commit-tag validation requires the `[<stem>]` tag — otherwise every
/// custom finish message trips `fix_commit_tag`. Idempotent: prepends
/// `[<stem>] ` only when the subject doesn't already start with `[<stem>]`;
/// the body is untouched. A subject that starts with a DIFFERENT `[other]`
/// tag still gets `[<stem>]` prepended (the rule keys on the plan's own stem).
fn ensure_plan_tag(message: &str, stem: &str) -> String {
    let tag = format!("[{stem}]");
    let subject = message.lines().next().unwrap_or("");
    if subject.starts_with(&tag) {
        message.to_string()
    } else {
        format!("{tag} {message}")
    }
}

/// Secondary guard only: catch a trivially-empty WHY body (e.g. `.` or a
/// stray word). The PRIMARY check is body PRESENCE — a real WHY always
/// clears this floor, so it never punishes a concise subject.
const MIN_WHY_BODY_CHARS: usize = 12;

/// Placeholder subjects that carry no summary. Compared case-insensitively
/// against the subject with any leading `[<stem>]` and trailing dots removed.
const FINISH_MESSAGE_PLACEHOLDERS: &[&str] =
    &["finish", "finished", "done", "wip", "complete", "completed"];

/// Enforce that the finish message reads like the whole plan's commit
/// message: a WHAT subject AND a WHY body. Rejects an absent/empty message,
/// the `[<stem>] finish` default, bare placeholders, and — the primary
/// check, per the review — a subject-only message (no WHY body). Keying on
/// the body rather than a subject-length floor is deliberate: a concise
/// subject with a real WHY passes, and a long subject with NO why is
/// rejected instead of landing silently.
fn validate_finish_message(message: Option<&str>, stem: &str) -> anyhow::Result<()> {
    let Some(msg) = message.map(str::trim).filter(|m| !m.is_empty()) else {
        anyhow::bail!("{}", finish_message_help(stem));
    };
    let subject = msg.lines().next().unwrap_or("").trim();
    let subject_core = subject
        .strip_prefix(&format!("[{stem}]"))
        .unwrap_or(subject)
        .trim()
        .trim_end_matches('.')
        .trim();
    let lower = subject_core.to_ascii_lowercase();
    if subject_core.is_empty() || FINISH_MESSAGE_PLACEHOLDERS.contains(&lower.as_str()) {
        anyhow::bail!("{}", finish_message_help(stem));
    }
    // PRIMARY: require a WHY body — content after the subject's blank line.
    let body = msg.split_once("\n\n").map(|(_, b)| b.trim()).unwrap_or("");
    if body.len() < MIN_WHY_BODY_CHARS {
        anyhow::bail!("{}", finish_message_help(stem));
    }
    Ok(())
}

fn finish_message_help(stem: &str) -> String {
    format!(
        "finish needs the whole plan's commit message: a brief subject saying \
         WHAT changed AND a body explaining the WHY (why the change exists — not \
         a restatement of the diff). A bare `finish` or a subject with no body is \
         rejected.\n\n  \
         clank finish {stem} -m \"<subject: what changed>\" -m \"<why it exists + effects>\"\n\n\
         The message becomes this plan's squash summary, so write it as if the \
         whole plan were a single commit."
    )
}

/// Dry-run preview for `finish --purge`/`--squash`. Emits a
/// description of the planned action without calling `finalize()`
/// or running the rewrite engine. Covers all three finalize
/// states: not-yet-finalized (the run would create the finalize),
/// already-finished + `--amend` (would re-commit it), and
/// already-finished without `--amend` (finalize left as-is; only
/// the range is rewritten). Not pipeable to `git` — for the
/// not-yet-finalized case the finalize commit doesn't exist yet,
/// so the operator must run the live command first.
fn dry_run_finish_composite(
    stem: &str,
    preview: &FinishPreviewResponse,
    args: &FinishArgs,
) -> anyhow::Result<()> {
    println!("# clank finish --dry preview");
    println!("# plan: {}", preview.plan_id);
    let already_finished = matches!(preview.readiness, FinalizeReadiness::AlreadyFinished);
    let msg = compose_finish_message(&args.message).unwrap_or_else(|| format!("[{stem}] finish"));
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
    // The squashed commit collapses the finalize rename in, so it touches the
    // plan file — tag the squash MSG so it doesn't trip `fix_commit_tag`
    // (ruthless 28e3be4).
    let squash_msg = args.squash.as_deref().map(|m| ensure_plan_tag(m, stem));
    crate::cli::rewrite::run(crate::cli::rewrite::RewriteOpts {
        repo,
        intro_sha: preview.intro_sha.as_ref(),
        head_sha: &preview.head_sha,
        linear: preview.linear,
        commits: &preview.commits,
        into_branch: args.into_branch.as_deref(),
        dry: args.dry,
        allow_rewrite_protected: args.allow_rewrite_protected,
        squash: squash_msg.as_deref(),
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
            clank_core::vocab::CommitGateState::Continued => {
                "latest reviewable commit is continued but not FINISHED — \
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
            clank_core::vocab::CommitGateState::ContinuedPendingGate => {
                "latest reviewable commit is continued by commit-tier reviewers; \
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
    let plan_path = repo.join(crate::init_facts::plan_md_rel(stem));
    let finished_path = finished_dir.join(format!("{stem}.md"));
    if plan_path.exists() {
        std::fs::copy(&plan_path, &finished_path)?;
    } else {
        // Plan file missing (e.g. hidden); write an empty finished marker.
        std::fs::write(&finished_path, "")?;
    }

    let rel_plan = crate::init_facts::plan_md_rel(stem);
    let rel_finished = crate::init_facts::finished_md_rel(stem);
    crate::git_plumbing::remove_path(repo, &rel_plan)?;
    crate::git_plumbing::stage(repo, &rel_finished)?;

    let default_msg = format!("[{stem}] finish");
    let msg = message.unwrap_or(&default_msg);
    crate::git_plumbing::commit(repo, msg, amend)?;
    Ok(())
}

fn amend_already_finished(repo: &Path, stem: &str, message: Option<&str>) -> anyhow::Result<()> {
    let default_msg = format!("[{stem}] finish");
    let msg = message.unwrap_or(&default_msg);
    crate::git_plumbing::commit(repo, msg, true)
}

fn require_head_is_finalize(repo: &Path, stem: &str) -> anyhow::Result<()> {
    if !head_is_finalize_for(repo, stem)? {
        anyhow::bail!(
            "rewriting plan `{stem}`'s finish commit needs it to be HEAD, but HEAD \
             is a different commit (work is stacked on top). Check out or rebase \
             onto the finalize commit, or edit it via an interactive rebase.",
        );
    }
    Ok(())
}

fn head_is_finalize_for(repo: &Path, stem: &str) -> anyhow::Result<bool> {
    let Some(head) = crate::git_io::rev_parse_head(repo)? else {
        return Ok(false);
    };
    let finished_path = crate::init_facts::finished_md_rel(stem);
    Ok(crate::git_io::diff_tree_changes_at(repo, &head)
        .map(|c| c.clank_paths_touched.contains(&finished_path))
        .unwrap_or(false))
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
            gate_state: CommitGateState::Continued,
            latest_reviewable_sha: None,
            plan_worktree_status: PlanWorktreeStatus::Clean,
            is_finished: false,
            sealed_approvals: vec![],
        }
    }

    #[test]
    fn finish_message_accepts_what_subject_plus_why_body() {
        // A real whole-plan message: WHAT subject + WHY body.
        assert!(
            validate_finish_message(
                Some("[foo] add retry on fetch\n\nthe daemon fetch flaked on transient DNS errors"),
                "foo",
            )
            .is_ok()
        );
        // A CONCISE subject is fine when the WHY body is present — the body is
        // the primary check, not subject length (ruthless 3018ae6).
        assert!(
            validate_finish_message(
                Some("add retry\n\nprevents a hang when the network blips mid-fetch"),
                "foo",
            )
            .is_ok()
        );
    }

    #[test]
    fn finish_message_rejects_absent_placeholder_and_default() {
        for bad in [
            None,
            Some(""),
            Some("   "),
            Some("finish"),
            Some("[foo] finish"), // the old default
            Some("done"),
            Some("WIP"),
        ] {
            assert!(
                validate_finish_message(bad, "foo").is_err(),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn finish_message_rejects_subject_only_even_when_long() {
        // The false-accept ruthless flagged: a long, descriptive subject with
        // NO why body must be rejected (not land silently).
        let long_subject_no_body = "refactor the entire authentication subsystem end to end";
        assert!(long_subject_no_body.len() > 40);
        assert!(validate_finish_message(Some(long_subject_no_body), "foo").is_err());
        // A trivially-empty body is also rejected (secondary floor).
        assert!(validate_finish_message(Some("real subject here\n\n."), "foo").is_err());
    }

    #[test]
    fn ensure_plan_tag_prepends_idempotently_and_preserves_body() {
        // Prepends the tag when the subject lacks it; body untouched.
        assert_eq!(
            ensure_plan_tag("do a thing\n\nbecause it was broken", "foo"),
            "[foo] do a thing\n\nbecause it was broken"
        );
        // Idempotent when already tagged.
        assert_eq!(
            ensure_plan_tag("[foo] do a thing\n\nwhy", "foo"),
            "[foo] do a thing\n\nwhy"
        );
        // A DIFFERENT `[other]` tag still gets `[foo]` prepended (the rule keys
        // on the plan's OWN stem — `[other]` doesn't satisfy it).
        assert_eq!(
            ensure_plan_tag("[other] x\n\nwhy", "foo"),
            "[foo] [other] x\n\nwhy"
        );
        // A similar-but-different stem is not mistaken for the tag.
        assert_eq!(
            ensure_plan_tag("[foobar] x\n\nwhy", "foo"),
            "[foo] [foobar] x\n\nwhy"
        );
    }

    #[test]
    fn finalize_commit_message_prefers_the_squash_landing_message() {
        // When squashing, the finalize/amend commit is stamped with the squash
        // MSG, so a refused squash leaves the validated message — not the
        // `[stem] finish` placeholder (codex 053f9d1).
        assert_eq!(finalize_commit_message(Some("sq"), Some("m")), Some("sq"));
        assert_eq!(finalize_commit_message(Some("sq"), None), Some("sq"));
        assert_eq!(finalize_commit_message(None, Some("m")), Some("m"));
        assert_eq!(finalize_commit_message(None, None), None);
    }

    #[test]
    fn compose_finish_message_joins_repeated_m_as_paragraphs() {
        assert_eq!(compose_finish_message(&[]), None);
        assert_eq!(
            compose_finish_message(&["subject".into()]).as_deref(),
            Some("subject")
        );
        assert_eq!(
            compose_finish_message(&["subject".into(), "why".into()]).as_deref(),
            Some("subject\n\nwhy"),
        );
    }

    /// Route to the message-that-lands, then validate it — the combined guard
    /// exactly as `run()` applies it.
    fn check(
        already_finished: bool,
        amend: bool,
        message: Option<&str>,
        squash: Option<&str>,
    ) -> anyhow::Result<()> {
        match message_requiring_validation(already_finished, amend, message, squash) {
            Some(m) => validate_finish_message(m, "foo"),
            None => Ok(()),
        }
    }

    #[test]
    fn squash_message_is_validated_not_the_transient_finalize_message() {
        // codex c2122b3: `--squash` MSG is the message that LANDS, so a
        // placeholder squash message must be rejected (the finalize commit it
        // creates first is squashed away).
        assert!(check(false, false, None, Some("finish")).is_err());
        assert!(check(false, false, None, Some("just a subject no body")).is_err());
        assert!(
            check(
                false,
                false,
                None,
                Some("collapse fetch retries\n\nso transient DNS blips don't wedge the daemon"),
            )
            .is_ok()
        );
        // The already-finished `--amend --squash` path has the same shape.
        assert!(check(true, true, None, Some("done")).is_err());
    }

    #[test]
    fn purge_finalize_still_requires_a_good_message() {
        // codex 53a9edb: `--purge` still CREATES a finalize commit that
        // survives a refused strip rewrite, so it must carry a validated
        // message — no exemption. (Routing is purge-agnostic: it treats purge
        // like any finalize-authoring path.)
        assert!(check(false, false, None, None).is_err()); // Ready + purge, no -m
        assert!(check(false, false, Some("finish"), None).is_err()); // placeholder
        assert!(
            check(
                false,
                false,
                Some("strip foo\n\nartifacts no longer needed"),
                None
            )
            .is_ok()
        );
    }

    #[test]
    fn plain_finalize_still_requires_a_good_message() {
        assert!(check(false, false, None, None).is_err()); // no -m
        assert!(check(false, false, Some("finish"), None).is_err()); // placeholder
        assert!(check(false, false, Some("add X\n\nbecause Y needed it"), None).is_ok());
        // A no-op finish on an already-finished plan validates nothing.
        assert!(check(true, false, None, None).is_ok());
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
        write_at(dir.path(), ".clank/finished/foo/codex.md", "CONTINUE\n");

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
