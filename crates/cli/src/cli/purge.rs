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

pub(crate) struct AmendProgram {
    pub head_sha: String,
    pub strip_paths: Vec<String>,
}

pub(crate) fn build_amend_program(
    repo: &std::path::Path,
    plan_key: Option<&PlanKey>,
) -> anyhow::Result<AmendProgram> {
    let head_files = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"])
        .output()?;
    let head_lines: Vec<String> = if head_files.status.success() {
        String::from_utf8_lossy(&head_files.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };
    let prefix: String = match plan_key {
        Some(k) => format!(".clank/finished/{}", k.as_str()),
        None => ".clank/finished/".to_string(),
    };
    let head_is_finalize =
        !head_lines.is_empty() && head_lines.iter().all(|l| l.starts_with(&prefix));
    if !head_is_finalize {
        anyhow::bail!(
            "--amend requires HEAD to be a finalize commit \
             (every changed path under `{prefix}`). Run `clank finish` \
             without `--amend` to create the finalize commit first, or \
             use `clank purge` without `--amend` to rewrite the chain."
        );
    }

    let head_out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()?;
    if !head_out.status.success() {
        anyhow::bail!("git rev-parse HEAD failed");
    }
    let head_sha = String::from_utf8(head_out.stdout)?.trim().to_string();

    let strip_paths: Vec<String> = match plan_key {
        Some(k) => {
            let s = k.as_str();
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args([
                    "ls-tree",
                    "-r",
                    "--name-only",
                    "--",
                    &head_sha,
                    &format!(".clank/plans/{s}.md"),
                    &format!(".clank/finished/{s}"),
                ])
                .output()?;
            if !out.status.success() {
                Vec::new()
            } else {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect()
            }
        }
        None => {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["ls-tree", "-r", "--name-only", "--", &head_sha, ".clank/"])
                .output()?;
            if !out.status.success() {
                Vec::new()
            } else {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect()
            }
        }
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
    let current_head = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()?;
    let current = String::from_utf8(current_head.stdout)?.trim().to_string();
    if current != program.head_sha {
        anyhow::bail!(
            "HEAD moved since the amend program was built \
             (expected {}, got {}). Re-run `clank purge --amend`.",
            &program.head_sha[..program.head_sha.len().min(7)],
            &current[..current.len().min(7)],
        );
    }

    for p in &program.strip_paths {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rm", "--cached", "-q", p])
            .status()?;
        if !status.success() {
            anyhow::bail!("git rm --cached {p} failed");
        }
    }
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "--amend", "--no-edit", "--allow-empty"])
        .status()?;
    if !status.success() {
        anyhow::bail!("git commit --amend failed");
    }
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

    let status_out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()?;
    if !status_out.status.success()
        || !String::from_utf8_lossy(&status_out.stdout)
            .trim()
            .is_empty()
    {
        anyhow::bail!("working tree dirty; commit or stash first");
    }
    if !args.allow_rewrite_protected {
        let current = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["symbolic-ref", "--short", "HEAD"])
            .output()?;
        let branch = String::from_utf8_lossy(&current.stdout).trim().to_string();
        let protected_by_name = matches!(branch.as_str(), "main" | "master");
        let protected_by_config = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["config", "--get", &format!("branch.{branch}.protect")])
            .output()
            .map(|o| {
                o.status.success()
                    && matches!(
                        String::from_utf8_lossy(&o.stdout).trim(),
                        "true" | "1" | "yes" | "on"
                    )
            })
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
        write_file(repo, ".clank/finished/foo", "");
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "--quiet", "-m", "Finalize foo"]);
        dir
    }

    #[test]
    fn build_amend_program_captures_head_and_strip_paths() {
        let dir = repo_with_finalize();
        let repo = dir.path();
        let plan = PlanKey::parse("foo").unwrap();
        let program = build_amend_program(repo, Some(&plan)).unwrap();
        assert_eq!(program.head_sha, head_sha(repo));
        assert!(
            program.strip_paths.iter().any(|p| p.contains("plans/foo")),
            "strip_paths should include plan file: {:?}",
            program.strip_paths
        );
        assert!(
            program
                .strip_paths
                .iter()
                .any(|p| p.contains("finished/foo")),
            "strip_paths should include finalize dir: {:?}",
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
}
