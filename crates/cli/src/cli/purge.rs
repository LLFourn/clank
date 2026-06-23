//! `clank purge` — strip a plan's `.clank/` artifacts from
//! history.
//!
//! Fully local: folds the repo with `rebuild::rebuild_repo`, builds
//! a typed rewrite preview via `crate::preview`, then feeds the
//! manifest to the shared rewrite engine (`cli::rewrite`). No
//! daemon required.

use std::io::Write;

use super::{PurgeArgs, repo_basename, resolve_repo};
use crate::cli::rewrite::{RewriteOpts, run as run_rewrite};
use crate::lifecycle::PlanKey;
use crate::rebuild::CachePolicy;
use clank_core::api::{RewriteCommit, RewriteDisposition};

fn cache_policy(args: &PurgeArgs) -> CachePolicy {
    if args.no_cache {
        CachePolicy::Bypass
    } else {
        CachePolicy::Use
    }
}

pub async fn run(args: PurgeArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;

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
    // `--drop` parse-time refusals per plan Phase 1.
    if args.drop && args.all {
        anyhow::bail!(
            "--drop and --all are mutually exclusive (not supported). \
             `--drop` operates per-plan; for a wholesale wipe, use individual \
             `clank purge <plan> --drop` invocations."
        );
    }
    if args.drop && args.squash.is_some() {
        anyhow::bail!(
            "--drop and --squash are mutually exclusive — you cannot squash commits you are dropping."
        );
    }
    if args.drop && args.amend {
        anyhow::bail!(
            "--drop and --amend are mutually exclusive — --amend rewrites HEAD's tree, --drop rewrites the whole chain."
        );
    }

    if args.amend {
        return run_amend(&repo, &basename, &args).await;
    }

    if args.all {
        run_all(&repo, &basename, &args).await
    } else {
        run_single(&repo, &basename, &args).await
    }
}

async fn run_single(
    repo: &std::path::Path,
    basename: &str,
    args: &PurgeArgs,
) -> anyhow::Result<()> {
    let state = crate::rebuild::rebuild_repo_with_policy(repo, cache_policy(args))
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
    let plan_key = crate::cli::plan_resolve::resolve_plan(&state, basename, args.plan.as_deref())?;
    let stem = plan_key.as_str().to_string();

    // Always strip the finalize snapshot too — if anyone wants to
    // preserve the audit trail, we can add `--keep-finalize` later.
    let preview = crate::preview::build_rewrite_preview(repo, &state, &plan_key, true)
        .await
        .map_err(|e| anyhow::anyhow!("rewrite preview failed: {e}"))?;

    // `--drop` transform: every non-foreign plan-attributed commit
    // becomes a `Drop`, so the implementation code goes away with
    // the `.clank/` artifacts. Foreign commits refuse before we
    // ever reach the engine. Plan Phase 2.
    let commits_for_engine = if args.drop {
        drop_safety_check(&preview.commits)?;
        transform_all_to_drop(&preview.commits)
    } else {
        preview.commits.clone()
    };

    // Confirmation prompt distinguishes the two modes per Phase 3.
    if !args.dry && !args.yes {
        let dropped_count = preview.commits.iter().filter(|c| !c.foreign).count();
        let ok = if args.drop {
            confirm_drop(&stem, dropped_count, args.into_branch.as_deref())?
        } else {
            confirm_single(&stem, args.into_branch.as_deref())?
        };
        if !ok {
            anyhow::bail!("aborted");
        }
    }

    let outcome = run_rewrite(RewriteOpts {
        repo,
        intro_sha: preview.intro_sha.as_ref(),
        head_sha: &preview.head_sha,
        linear: preview.linear,
        commits: &commits_for_engine,
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

async fn run_all(repo: &std::path::Path, basename: &str, args: &PurgeArgs) -> anyhow::Result<()> {
    let preview = crate::preview::build_rewrite_preview_all(repo, true)
        .await
        .map_err(|e| anyhow::anyhow!("all-rewrite preview failed: {e}"))?;
    if preview.intro_sha.is_none() {
        println!("no .clank/ history found in `{basename}`; nothing to purge.");
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
            "purged all .clank/ history: branch `{branch}` now at {} ({})",
            &tip[..tip.len().min(7)],
            tip,
        );
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct AmendProgram {
    pub head_sha: String,
    pub strip_paths: Vec<String>,
}

pub(crate) fn build_amend_program(
    repo: &std::path::Path,
    plan_key: Option<&PlanKey>,
) -> anyhow::Result<AmendProgram> {
    let head_lines = crate::git_io::diff_tree_name_status(repo, true).unwrap_or_default();
    let head_is_finalize = is_finish_diff(&head_lines, plan_key);
    if !head_is_finalize {
        let desc = match plan_key {
            Some(k) => format!(".clank/finished/{}.md", k.as_str()),
            None => ".clank/finished/".to_string(),
        };
        anyhow::bail!(
            "--amend requires HEAD to be a finalize commit \
             (exact plans/ → finished/ move for `{desc}`). Run `clank finish` \
             without `--amend` to create the finalize commit first, or \
             use `clank purge` without `--amend` to rewrite the chain."
        );
    }

    let head = crate::git_io::rev_parse_head(repo)?
        .ok_or_else(|| anyhow::anyhow!("git rev-parse HEAD failed"))?;
    let head_sha = head.as_str().to_string();

    let strip_paths: Vec<String> = match plan_key {
        // The plan's own artifact at HEAD — whichever of plans/ or
        // finished/ actually exists in the tree.
        Some(k) => {
            let s = k.as_str();
            let plan_p = format!(".clank/plans/{s}.md");
            let finished_p = format!(".clank/finished/{s}.md");
            crate::git_io::tree_clank_paths(repo, &head)
                .unwrap_or_default()
                .into_iter()
                .filter(|p| *p == plan_p || *p == finished_p)
                .collect()
        }
        // Whole-repo amend: every `.clank/` path in HEAD's tree.
        None => crate::git_io::tree_clank_paths(repo, &head).unwrap_or_default(),
    };

    Ok(AmendProgram {
        head_sha,
        strip_paths,
    })
}

pub(crate) fn render_amend_dry(program: &AmendProgram) -> String {
    let mut out = String::new();
    out.push_str("# clank purge --amend preview\n");
    out.push_str(&format!("# HEAD: {}\n", program.head_sha));
    out.push_str("# would strip from HEAD's tree:\n");
    for p in &program.strip_paths {
        out.push_str(&format!("#   - {p}\n"));
    }
    out.push_str("# (--dry: no commits, no refs touched)\n");
    out
}

pub(crate) fn execute_amend(repo: &std::path::Path, program: &AmendProgram) -> anyhow::Result<()> {
    let current = crate::git_io::rev_parse_head(repo)?
        .map(|s| s.as_str().to_string())
        .unwrap_or_default();
    if current != program.head_sha {
        anyhow::bail!(
            "HEAD moved since the amend program was built \
             (expected {}, got {}). Re-run `clank purge --amend`.",
            &program.head_sha[..program.head_sha.len().min(7)],
            &current[..current.len().min(7)],
        );
    }

    for p in &program.strip_paths {
        crate::git_plumbing::remove_cached(repo, p)?;
    }
    crate::git_plumbing::amend_no_edit(repo)?;
    println!(
        "amended HEAD: stripped {} path(s)",
        program.strip_paths.len()
    );
    Ok(())
}

/// `--amend`: strip the plan's `.clank/` paths from HEAD's tree
/// and amend HEAD (no chain rewrite). Useful when the operator
/// has just landed a commit and wants to retroactively scrub the
/// plan's artifacts from HEAD's tree without rewriting earlier
/// history.
async fn run_amend(repo: &std::path::Path, basename: &str, args: &PurgeArgs) -> anyhow::Result<()> {
    let plan_key: Option<PlanKey> = if args.all {
        None
    } else {
        let state = crate::rebuild::rebuild_repo_with_policy(repo, cache_policy(args))
            .await
            .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
        Some(crate::cli::plan_resolve::resolve_plan(
            &state,
            basename,
            args.plan.as_deref(),
        )?)
    };

    let program = build_amend_program(repo, plan_key.as_ref())?;

    if program.strip_paths.is_empty() {
        println!("HEAD's tree has no strippable Clank paths; nothing to amend");
        return Ok(());
    }

    if !crate::git_io::working_tree_clean(repo)? {
        anyhow::bail!("working tree dirty; commit or stash first");
    }
    if !args.allow_rewrite_protected {
        // Detached HEAD → no branch name → not protected-by-name
        // (matches the old empty `symbolic-ref` output).
        let branch = crate::git_io::current_branch_at(repo)
            .ok()
            .flatten()
            .unwrap_or_default();
        let protected_by_name = matches!(branch.as_str(), "main" | "master");
        let protected_by_config =
            crate::git_io::config_bool(repo, &format!("branch.{branch}.protect"))
                .ok()
                .flatten()
                .unwrap_or(false);
        if protected_by_name || protected_by_config {
            anyhow::bail!(
                "refusing to amend protected branch `{branch}`. \
                 Pass `--allow-rewrite-protected` to override."
            );
        }
    }

    if args.dry {
        print!("{}", render_amend_dry(&program));
        return Ok(());
    }

    if !args.yes
        && !confirm_with(&format!(
            "About to amend HEAD: strip {} path(s) from HEAD's tree.",
            program.strip_paths.len()
        ))?
    {
        anyhow::bail!("aborted");
    }

    execute_amend(repo, &program)?;
    Ok(())
}

/// A finish commit is one that only touches `.clank/plans/` and
/// `.clank/finished/` paths and adds at least one file to `finished/`.
fn is_finish_diff(lines: &[String], plan_key: Option<&PlanKey>) -> bool {
    if lines.is_empty() {
        return false;
    }
    let mut has_finished_add = false;
    for line in lines {
        let mut parts = line.splitn(3, '\t');
        let status = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");
        let path2 = parts.next();
        let sc = status.chars().next().unwrap_or(' ');
        let relevant_path = if sc == 'R' { path2.unwrap_or("") } else { path };
        let in_plans = relevant_path.starts_with(".clank/plans/");
        let in_finished = relevant_path.starts_with(".clank/finished/");
        if !in_plans && !in_finished {
            // Also check the old side of renames
            if sc == 'R' && path.starts_with(".clank/plans/") {
                // old side in plans, new side checked above
            } else {
                return false;
            }
        }
        if in_finished && sc != 'D' {
            if let Some(stem) = relevant_path
                .strip_prefix(".clank/finished/")
                .and_then(|s| s.strip_suffix(".md"))
            {
                if plan_key.is_none_or(|k| k.as_str() == stem) {
                    has_finished_add = true;
                }
            }
        }
    }
    has_finished_add
}

/// Foreign-commit refusal under `--drop`. Mirrors the foreign
/// arm of `cli::shelve::safety_check`. The `Rewrite` tier is
/// NOT enforced under `--drop` — the `--drop` flag itself is
/// the opt-in to losing code (Phase 2, OQ1 tentative pick:
/// skip the Rewrite-requires-force tier).
fn drop_safety_check(commits: &[RewriteCommit]) -> anyhow::Result<()> {
    let foreign: Vec<&clank_core::ids::CommitSha> = commits
        .iter()
        .filter(|c| c.foreign)
        .map(|c| &c.sha)
        .collect();
    if !foreign.is_empty() {
        let list: Vec<String> = foreign.iter().map(|s| s.as_str().to_string()).collect();
        anyhow::bail!(
            "purge --drop refuses: foreign commit(s) interleaved in plan range: {}. \
             Foreign commits are someone else's work; dropping them via this command \
             would silently rewrite their history. Resolve via `git rebase -i` or \
             coordinate with the foreign-commit author(s).",
            list.join(", ")
        );
    }
    Ok(())
}

/// Transform every non-foreign plan-attributed commit's
/// disposition to `Drop`. Foreign commits are preserved as-is
/// (gated out by `drop_safety_check` above; this is defensive).
/// Mirrors `cli::shelve.rs`'s transform.
fn transform_all_to_drop(commits: &[RewriteCommit]) -> Vec<RewriteCommit> {
    commits
        .iter()
        .map(|c| {
            if c.foreign {
                c.clone()
            } else {
                RewriteCommit {
                    sha: c.sha.clone(),
                    subject: c.subject.clone(),
                    disposition: RewriteDisposition::Drop,
                    foreign: false,
                    strip_paths: c.strip_paths.clone(),
                }
            }
        })
        .collect()
}

fn confirm_drop(
    stem: &str,
    dropped_count: usize,
    into_branch: Option<&str>,
) -> anyhow::Result<bool> {
    let target = match into_branch {
        Some(b) => format!("a NEW branch `{b}` (current branch left untouched)"),
        None => "the CURRENT branch (history will be rewritten in place)".into(),
    };
    let prompt = format!(
        "About to DROP all {dropped_count} commit(s) attributed to `{stem}` and write \
         rewritten history to {target}. \
         This deletes the implementation code, not just `.clank/` artifacts. \
         There is NO archived copy of the plan body anywhere — for that, use `clank shelve --to-queue`. \
         Continue? [y/N] "
    );
    confirm_with(&prompt)
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
        "About to purge EVERY `.clank/` path from history \
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
    async fn all_with_plan_arg_errors_before_fold() {
        let args = PurgeArgs {
            plan: Some("foo".into()),
            all: true,
            repo: Some(std::env::current_dir().unwrap()),
            into_branch: None,
            dry: false,
            yes: true,
            squash: None,
            amend: false,
            allow_rewrite_protected: false,
            no_cache: false,
            drop: false,
        };
        let err = run(args).await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("mutually exclusive"),
            "expected mutual-exclusion error, got: {msg}"
        );
    }

    fn purge_drop_args(plan: Option<&str>) -> PurgeArgs {
        PurgeArgs {
            plan: plan.map(String::from),
            all: false,
            repo: Some(std::env::current_dir().unwrap()),
            into_branch: None,
            dry: false,
            yes: true,
            squash: None,
            amend: false,
            allow_rewrite_protected: false,
            no_cache: false,
            drop: true,
        }
    }

    #[tokio::test]
    async fn drop_with_all_errors_at_parse_time() {
        let mut args = purge_drop_args(None);
        args.all = true;
        let err = run(args).await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("--drop and --all are mutually exclusive"),
            "expected --drop+--all refusal; got: {msg}"
        );
    }

    #[tokio::test]
    async fn drop_with_squash_errors_at_parse_time() {
        let mut args = purge_drop_args(Some("foo"));
        args.squash = Some("squash msg".into());
        let err = run(args).await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("--drop and --squash are mutually exclusive"),
            "expected --drop+--squash refusal; got: {msg}"
        );
    }

    #[tokio::test]
    async fn drop_with_amend_errors_at_parse_time() {
        let mut args = purge_drop_args(Some("foo"));
        args.amend = true;
        let err = run(args).await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("--drop and --amend are mutually exclusive"),
            "expected --drop+--amend refusal; got: {msg}"
        );
    }

    #[test]
    fn drop_safety_check_refuses_foreign_unconditionally() {
        use clank_core::api::{RewriteCommit, RewriteDisposition};
        use clank_core::ids::CommitSha;
        let foreign = RewriteCommit {
            sha: CommitSha::parse(&"a".repeat(40)).unwrap(),
            subject: "foreign".into(),
            disposition: RewriteDisposition::KeepVerbatim,
            foreign: true,
            strip_paths: Vec::new(),
        };
        let err = drop_safety_check(&[foreign]).expect_err("must refuse");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("foreign") && msg.contains("git rebase"),
            "diagnostic should mention foreign + git rebase; got: {msg}"
        );
    }

    #[test]
    fn drop_safety_check_passes_all_drop_no_foreign() {
        use clank_core::api::{RewriteCommit, RewriteDisposition};
        use clank_core::ids::CommitSha;
        let c = RewriteCommit {
            sha: CommitSha::parse(&"b".repeat(40)).unwrap(),
            subject: "plan".into(),
            disposition: RewriteDisposition::Drop,
            foreign: false,
            strip_paths: Vec::new(),
        };
        drop_safety_check(&[c]).expect("non-foreign Drop should pass");
    }

    #[test]
    fn drop_safety_check_passes_rewrite_non_foreign_without_force() {
        // OQ1 pinned: --drop is the opt-in; Rewrite tier is NOT
        // enforced under --drop. (Contrast with `clank shelve`
        // which requires --force for non-foreign Rewrite.)
        use clank_core::api::{RewriteCommit, RewriteDisposition};
        use clank_core::ids::CommitSha;
        let c = RewriteCommit {
            sha: CommitSha::parse(&"c".repeat(40)).unwrap(),
            subject: "mixed".into(),
            disposition: RewriteDisposition::Rewrite,
            foreign: false,
            strip_paths: Vec::new(),
        };
        drop_safety_check(&[c])
            .expect("Rewrite non-foreign must pass under --drop without a second force flag");
    }

    #[test]
    fn transform_all_to_drop_preserves_foreign() {
        use clank_core::api::{RewriteCommit, RewriteDisposition};
        use clank_core::ids::CommitSha;
        let plan = RewriteCommit {
            sha: CommitSha::parse(&"d".repeat(40)).unwrap(),
            subject: "plan".into(),
            disposition: RewriteDisposition::Rewrite,
            foreign: false,
            strip_paths: Vec::new(),
        };
        let foreign = RewriteCommit {
            sha: CommitSha::parse(&"e".repeat(40)).unwrap(),
            subject: "foreign".into(),
            disposition: RewriteDisposition::KeepVerbatim,
            foreign: true,
            strip_paths: Vec::new(),
        };
        let out = transform_all_to_drop(&[plan, foreign]);
        assert_eq!(out[0].disposition, RewriteDisposition::Drop);
        assert!(matches!(
            out[1].disposition,
            RewriteDisposition::KeepVerbatim
        ));
        assert!(out[1].foreign);
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

    use std::path::Path;

    fn git(repo: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn write_file(repo: &Path, rel: &str, body: &str) {
        let abs = repo.join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, body).unwrap();
    }

    fn head_sha(repo: &Path) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn init_test_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        git(path, &["init", "--quiet", "--initial-branch=work"]);
        git(path, &["config", "user.email", "test@test"]);
        git(path, &["config", "user.name", "test"]);
        git(path, &["config", "commit.gpgsign", "false"]);
        dir
    }

    /// Create a repo with a plan intro, an impl commit, and a
    fn repo_with_finalize() -> tempfile::TempDir {
        let dir = init_test_repo();
        let repo = dir.path();
        write_file(repo, ".clank/plans/foo.md", "# foo\n");
        write_file(repo, "src/lib.rs", "// impl\n");
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "--quiet", "-m", "[foo] intro + impl"]);
        // Move plan file to finished/ to trigger Finish detection.
        write_file(repo, ".clank/finished/foo.md", "# foo\n");
        git(repo, &["rm", "--quiet", ".clank/plans/foo.md"]);
        git(repo, &["add", ".clank/finished/foo.md"]);
        git(repo, &["commit", "--quiet", "-m", "[foo] finish"]);
        dir
    }

    #[test]
    fn build_amend_program_captures_head_and_strip_paths() {
        let dir = repo_with_finalize();
        let repo = dir.path();
        let plan = PlanKey::parse("foo").unwrap();
        let program = build_amend_program(repo, Some(&plan)).unwrap();
        assert_eq!(program.head_sha, head_sha(repo));
        // After mv-finish, plans/foo.md is deleted and finished/foo.md is
        // present in the HEAD tree. Only the finished path needs stripping.
        assert!(
            program
                .strip_paths
                .iter()
                .any(|p| p.contains("finished/foo.md")),
            "strip_paths should include finalize file: {:?}",
            program.strip_paths
        );
    }

    #[test]
    fn render_dry_mentions_every_strip_path() {
        let dir = repo_with_finalize();
        let repo = dir.path();
        let plan = PlanKey::parse("foo").unwrap();
        let program = build_amend_program(repo, Some(&plan)).unwrap();

        let rendered = render_amend_dry(&program);
        assert!(
            rendered.contains(&program.head_sha),
            "render should include HEAD SHA; got:\n{rendered}"
        );
        for p in &program.strip_paths {
            assert!(
                rendered.contains(p),
                "render missing strip path `{p}`; got:\n{rendered}"
            );
        }
        assert!(
            rendered.contains("--dry: no commits"),
            "render should include dry-run notice; got:\n{rendered}"
        );
    }

    #[test]
    fn execute_amend_strips_paths_from_head() {
        let dir = repo_with_finalize();
        let repo = dir.path();
        let plan = PlanKey::parse("foo").unwrap();
        let program = build_amend_program(repo, Some(&plan)).unwrap();
        execute_amend(repo, &program).unwrap();

        let tree_out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["ls-tree", "-r", "--name-only", "HEAD"])
            .output()
            .unwrap();
        let tree = String::from_utf8_lossy(&tree_out.stdout).to_string();
        for p in &program.strip_paths {
            assert!(
                !tree.contains(p),
                "path `{p}` should have been stripped from HEAD's tree; tree:\n{tree}"
            );
        }
        assert!(
            tree.contains("src/lib.rs"),
            "non-clank files should be preserved; tree:\n{tree}"
        );
    }

    #[test]
    fn execute_amend_bails_on_stale_head() {
        let dir = repo_with_finalize();
        let repo = dir.path();
        let plan = PlanKey::parse("foo").unwrap();
        let program = build_amend_program(repo, Some(&plan)).unwrap();

        // Move HEAD by committing something new.
        write_file(repo, "stale.txt", "move head\n");
        git(repo, &["add", "stale.txt"]);
        git(repo, &["commit", "--quiet", "-m", "move head"]);

        let err = execute_amend(repo, &program).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("HEAD moved"),
            "expected stale-HEAD error; got: {msg}"
        );
    }

    #[test]
    fn build_amend_program_rejects_prefix_colliding_finalize() {
        let dir = init_test_repo();
        let repo = dir.path();
        write_file(repo, ".clank/plans/foo.md", "# foo\n");
        write_file(repo, ".clank/plans/foobar.md", "# foobar\n");
        write_file(repo, "src/lib.rs", "// impl\n");
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "--quiet", "-m", "[foo,foobar] intro"]);
        // Finalize foobar (not foo) using the mv-finish approach.
        write_file(repo, ".clank/finished/foobar.md", "# foobar\n");
        git(repo, &["rm", "--quiet", ".clank/plans/foobar.md"]);
        git(repo, &["add", ".clank/finished/foobar.md"]);
        git(repo, &["commit", "--quiet", "-m", "[foobar] finish"]);

        let foo = PlanKey::parse("foo").unwrap();
        let err = build_amend_program(repo, Some(&foo)).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("--amend requires HEAD to be a finalize commit"),
            "should reject foobar finalize when purging foo; got: {msg}"
        );
    }
}
