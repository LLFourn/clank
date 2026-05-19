//! Shared history-rewriting engine for `trinity purge` and
//! `trinity finish --purge`. Consumes the daemon's typed
//! `RewritePreviewResponse` and applies it via git plumbing.
//!
//! The engine never re-derives Trinity attribution or classification —
//! the daemon's manifest IS the rewrite plan. This module is
//! pure side-effect-producing infrastructure (git plumbing,
//! ref updates, tree builds).

use std::path::Path;

use trinity_core::api::{RewriteCommit, RewriteDisposition, RewritePreviewResponse};

/// Engine inputs. `dry == true` makes the engine compute everything
/// up to the first `commit-tree` / `update-ref` call and then exit
/// without producing any git objects or moving refs.
#[derive(Debug)]
pub struct RewriteOpts<'a> {
    pub repo: &'a Path,
    pub preview: &'a RewritePreviewResponse,
    /// `None` rewrites the current branch in place; `Some(name)`
    /// writes the rewritten chain to a fresh branch. Refuses if the
    /// branch already exists.
    pub into_branch: Option<&'a str>,
    pub dry: bool,
}

#[derive(Debug, Default)]
pub struct RewriteOutcome {
    /// SHA of the new tip after rewriting. `None` in dry mode.
    pub new_tip: Option<String>,
    /// Branch the engine updated. `None` in dry mode.
    pub updated_branch: Option<String>,
}

/// Drive the rewrite. Engine refuses pre-flight on:
/// - non-linear range (merge commit in [`intro_sha`, `head_sha`])
/// - dirty working tree
/// - `--into-branch` pointing at an existing branch
pub async fn run(opts: RewriteOpts<'_>) -> anyhow::Result<RewriteOutcome> {
    let preview = opts.preview;

    if !preview.linear {
        anyhow::bail!(
            "range contains a merge commit; refusing to rewrite (merge-tree \
             rewriting is out of scope)",
        );
    }
    let Some(intro) = preview.intro_sha.as_ref() else {
        anyhow::bail!("plan has no intro commit; nothing to rewrite");
    };

    if working_tree_dirty(opts.repo)? {
        anyhow::bail!("working tree dirty; commit or stash first");
    }

    if let Some(branch) = opts.into_branch
        && branch_exists(opts.repo, branch)?
    {
        anyhow::bail!(
            "branch `{branch}` already exists; refusing to overwrite. \
             Pick a different name or delete it first.",
        );
    }

    let intro_parent = parent_of(opts.repo, intro.as_str())?;
    let plan = build_plan(&preview.commits, intro_parent.as_deref());

    if opts.dry {
        print_dry_run(preview, &plan);
        return Ok(RewriteOutcome::default());
    }

    let new_tip = apply_plan(opts.repo, &plan).await?;
    let updated_branch = match opts.into_branch {
        Some(name) => {
            git_run(
                opts.repo,
                &["update-ref", &format!("refs/heads/{name}"), &new_tip],
            )?;
            name.to_string()
        }
        None => {
            let current = current_branch(opts.repo)?;
            git_run(
                opts.repo,
                &["update-ref", &format!("refs/heads/{current}"), &new_tip],
            )?;
            // Re-sync the worktree to the new tip without losing
            // anything (we already refused on dirty above).
            git_run(opts.repo, &["reset", "--hard", "HEAD"])?;
            current
        }
    };

    Ok(RewriteOutcome {
        new_tip: Some(new_tip),
        updated_branch: Some(updated_branch),
    })
}

#[derive(Debug)]
struct PlanStep {
    sha: String,
    subject: String,
    disposition: RewriteDisposition,
    foreign: bool,
    strip_paths: Vec<String>,
}

#[derive(Debug)]
struct ExecutionPlan {
    intro_parent: Option<String>,
    steps: Vec<PlanStep>,
}

fn build_plan(commits: &[RewriteCommit], intro_parent: Option<&str>) -> ExecutionPlan {
    ExecutionPlan {
        intro_parent: intro_parent.map(str::to_string),
        steps: commits
            .iter()
            .map(|c| PlanStep {
                sha: c.sha.as_str().to_string(),
                subject: c.subject.clone(),
                disposition: c.disposition,
                foreign: c.foreign,
                strip_paths: c.strip_paths.clone(),
            })
            .collect(),
    }
}

fn print_dry_run(preview: &RewritePreviewResponse, plan: &ExecutionPlan) {
    println!(
        "dry-run: would rewrite {} commit(s) from {} to {}",
        plan.steps.len(),
        preview
            .intro_sha
            .as_ref()
            .map(|s| short(s.as_str()))
            .unwrap_or_else(|| "(none)".into()),
        short(preview.head_sha.as_str()),
    );
    println!(
        "starting parent: {}",
        plan.intro_parent.as_deref().unwrap_or("(root)")
    );
    let foreign_count = plan.steps.iter().filter(|s| s.foreign).count();
    if foreign_count > 0 {
        println!(
            "  warning: {foreign_count} commit(s) in range are NOT attributed to this plan; \
             they will be kept verbatim (or rewritten if cross-plan mixed)"
        );
    }
    for step in &plan.steps {
        let action = match step.disposition {
            RewriteDisposition::Drop => "drop",
            RewriteDisposition::KeepVerbatim => "keep verbatim",
            RewriteDisposition::Rewrite => "rewrite",
        };
        let foreign_tag = if step.foreign { " (foreign)" } else { "" };
        println!(
            "  {} {}{}  {}",
            short(&step.sha),
            action,
            foreign_tag,
            step.subject
        );
        for path in &step.strip_paths {
            println!("    - strip {path}");
        }
    }
    println!("(dry-run: no commits created, no refs updated)");
}

/// Walk `plan.steps` in order, producing a new tip SHA. Returns the
/// final tip (parent of the next-to-be-rewritten commit, or the
/// intro_parent if every step was dropped).
async fn apply_plan(repo: &Path, plan: &ExecutionPlan) -> anyhow::Result<String> {
    let mut parent: Option<String> = plan.intro_parent.clone();
    for step in &plan.steps {
        match step.disposition {
            RewriteDisposition::Drop => {
                // Parent chain hops over this commit.
            }
            RewriteDisposition::KeepVerbatim => {
                // Re-commit with the same tree but our running parent
                // (since predecessors may have shifted). If our parent
                // equals this commit's natural parent, we could reuse
                // the SHA — but rebuilding is uniformly correct and
                // costs one commit-tree call.
                let tree = git_capture(repo, &["rev-parse", &format!("{}^{{tree}}", step.sha)])?;
                let new_sha =
                    commit_tree_preserving_meta(repo, &step.sha, &tree, parent.as_deref())?;
                parent = Some(new_sha);
            }
            RewriteDisposition::Rewrite => {
                let new_tree = build_stripped_tree(repo, &step.sha, &step.strip_paths)?;
                let new_sha =
                    commit_tree_preserving_meta(repo, &step.sha, &new_tree, parent.as_deref())?;
                parent = Some(new_sha);
            }
        }
    }
    parent.ok_or_else(|| anyhow::anyhow!("rewrite produced no commits (every step dropped?)"))
}

/// Build a new tree object from `<sha>`'s tree by removing each path
/// in `strip_paths`. Uses a scratch git index under `<repo>/.git/`
/// so we never disturb the operator's real index.
fn build_stripped_tree(repo: &Path, sha: &str, strip_paths: &[String]) -> anyhow::Result<String> {
    let scratch_dir = repo.join(".git").join("trinity-rewrite");
    std::fs::create_dir_all(&scratch_dir)?;
    let unique = format!(
        "rewrite-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    let index_path = scratch_dir.join(unique);
    // Belt-and-braces cleanup: if a previous run died here, an
    // ancient empty index could confuse `read-tree`.
    let _ = std::fs::remove_file(&index_path);

    let result = (|| -> anyhow::Result<String> {
        git_run_with_index(
            repo,
            &index_path,
            &["read-tree", &format!("{sha}^{{tree}}")],
        )?;
        for path in strip_paths {
            git_run_with_index(
                repo,
                &index_path,
                &["update-index", "--remove", "--force-remove", path],
            )?;
        }
        let tree = git_capture_with_index(repo, &index_path, &["write-tree"])?;
        Ok(tree.trim().to_string())
    })();

    let _ = std::fs::remove_file(&index_path);
    result
}

/// Commit a tree preserving the original commit's author, message,
/// and timestamps. The committer-side fields refresh — we want the
/// rewrite to be attributable to the rewriter.
fn commit_tree_preserving_meta(
    repo: &Path,
    original_sha: &str,
    tree_sha: &str,
    parent: Option<&str>,
) -> anyhow::Result<String> {
    let author = git_capture(repo, &["show", "-s", "--format=%an <%ae>", original_sha])?;
    let author_date = git_capture(repo, &["show", "-s", "--format=%aI", original_sha])?;
    let raw_msg = git_capture(repo, &["show", "-s", "--format=%B", original_sha])?;

    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(repo);
    cmd.env("GIT_AUTHOR_NAME", "");
    cmd.env("GIT_AUTHOR_EMAIL", "");
    cmd.env_remove("GIT_AUTHOR_NAME");
    cmd.env_remove("GIT_AUTHOR_EMAIL");
    cmd.env("GIT_AUTHOR_DATE", author_date.trim());
    let author = author.trim();
    if let Some((name, email)) = parse_author(author) {
        cmd.env("GIT_AUTHOR_NAME", name);
        cmd.env("GIT_AUTHOR_EMAIL", email);
    }
    cmd.args(["commit-tree", tree_sha]);
    if let Some(p) = parent {
        cmd.args(["-p", p]);
    }
    cmd.arg("-m").arg(raw_msg.trim_end());
    let output = cmd.output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git commit-tree failed for {original_sha}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn parse_author(spec: &str) -> Option<(&str, &str)> {
    let (name, rest) = spec.rsplit_once(" <")?;
    let email = rest.trim_end_matches('>');
    Some((name, email))
}

fn working_tree_dirty(repo: &Path) -> anyhow::Result<bool> {
    let stdout = git_capture(repo, &["status", "--porcelain"])?;
    Ok(!stdout.trim().is_empty())
}

fn branch_exists(repo: &Path, branch: &str) -> anyhow::Result<bool> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .output()?;
    Ok(output.status.success())
}

fn parent_of(repo: &Path, sha: &str) -> anyhow::Result<Option<String>> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", &format!("{sha}^")])
        .output()?;
    if !output.status.success() {
        // `<sha>^` fails when sha is a root commit.
        return Ok(None);
    }
    Ok(Some(String::from_utf8(output.stdout)?.trim().to_string()))
}

fn current_branch(repo: &Path) -> anyhow::Result<String> {
    let stdout = git_capture(repo, &["symbolic-ref", "--short", "HEAD"])?;
    Ok(stdout.trim().to_string())
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

fn git_capture(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn git_run_with_index(repo: &Path, index: &Path, args: &[&str]) -> anyhow::Result<()> {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .env("GIT_INDEX_FILE", index)
        .args(args)
        .status()?;
    if !status.success() {
        anyhow::bail!(
            "git {} (with private index) failed (exit {})",
            args.join(" "),
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

fn git_capture_with_index(repo: &Path, index: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .env("GIT_INDEX_FILE", index)
        .args(args)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} (with private index) failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use trinity_core::api::{RewriteCommit, RewriteDisposition, RewritePreviewResponse};
    use trinity_core::ids::{CommitSha, PlanKey};

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "--quiet", "--initial-branch=main"]);
        run(dir.path(), &["config", "user.email", "test@test"]);
        run(dir.path(), &["config", "user.name", "test"]);
        run(dir.path(), &["config", "commit.gpgsign", "false"]);
        dir
    }

    fn run(cwd: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn write(repo: &Path, rel: &str, body: &str) {
        let p = repo.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn commit(repo: &Path, msg: &str) -> String {
        run(repo, &["add", "-A"]);
        run(repo, &["commit", "--quiet", "-m", msg]);
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn show_subject(repo: &Path, sha: &str) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["show", "-s", "--format=%s", sha])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn rev_list(repo: &Path, branch: &str) -> Vec<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-list", "--reverse", branch])
            .output()
            .unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn tree_has(repo: &Path, sha: &str, path: &str) -> bool {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["ls-tree", "-r", "--name-only", sha])
            .output()
            .unwrap();
        let listing = String::from_utf8(out.stdout).unwrap();
        listing.lines().any(|l| l == path)
    }

    fn mk_preview(
        intro: &str,
        head: &str,
        commits: Vec<(String, String, RewriteDisposition, bool, Vec<String>)>,
    ) -> RewritePreviewResponse {
        RewritePreviewResponse {
            plan_id: "trinity/foo.md".into(),
            plan_stem: PlanKey::parse("foo").unwrap(),
            intro_sha: Some(CommitSha::parse(intro).unwrap()),
            head_sha: CommitSha::parse(head).unwrap(),
            linear: true,
            commits: commits
                .into_iter()
                .map(|(sha, subj, disp, foreign, strip)| RewriteCommit {
                    sha: CommitSha::parse(&sha).unwrap(),
                    subject: subj,
                    disposition: disp,
                    foreign,
                    strip_paths: strip,
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn rewrite_into_branch_strips_plan_file_from_mixed_commit() {
        let dir = init_repo();
        // First-ever commit so the engine has an intro_parent of None.
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
        write(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        write(dir.path(), ".trinity/plans/foo.md", "# foo v2\n");
        write(dir.path(), "src/lib.rs", "// code\n");
        let mixed = commit(dir.path(), "mixed: revise foo + code");

        let preview = mk_preview(
            &intro,
            &mixed,
            vec![
                (
                    intro.clone(),
                    "plan: foo".into(),
                    RewriteDisposition::Drop,
                    false,
                    vec![],
                ),
                (
                    mixed.clone(),
                    "mixed: revise foo + code".into(),
                    RewriteDisposition::Rewrite,
                    false,
                    vec![".trinity/plans/foo.md".into()],
                ),
            ],
        );
        let outcome = super::run(RewriteOpts {
            repo: dir.path(),
            preview: &preview,
            into_branch: Some("purged"),
            dry: false,
        })
        .await
        .unwrap();
        let new_tip = outcome.new_tip.unwrap();
        assert_eq!(outcome.updated_branch.as_deref(), Some("purged"));

        // The rewritten chain on `purged` should have exactly one new
        // commit after the seed (because intro was Drop).
        let chain = rev_list(dir.path(), "purged");
        assert_eq!(chain.len(), 2, "seed + one rewritten commit; got {chain:?}");
        assert_eq!(chain.last().unwrap(), &new_tip);
        // The rewritten tip preserves the original subject.
        assert_eq!(
            show_subject(dir.path(), &new_tip),
            "mixed: revise foo + code"
        );
        // ...but its tree omits .trinity/plans/foo.md and keeps src/lib.rs.
        assert!(!tree_has(dir.path(), &new_tip, ".trinity/plans/foo.md"));
        assert!(tree_has(dir.path(), &new_tip, "src/lib.rs"));
        // The original `main` branch is untouched.
        let main_chain = rev_list(dir.path(), "main");
        assert!(main_chain.contains(&mixed));
    }

    #[tokio::test]
    async fn rewrite_dry_run_makes_no_commits_or_refs() {
        let dir = init_repo();
        write(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        let preview = mk_preview(
            &intro,
            &intro,
            vec![(
                intro.clone(),
                "plan: foo".into(),
                RewriteDisposition::Drop,
                false,
                vec![],
            )],
        );
        let outcome = super::run(RewriteOpts {
            repo: dir.path(),
            preview: &preview,
            into_branch: Some("dry-target"),
            dry: true,
        })
        .await
        .unwrap();
        assert!(outcome.new_tip.is_none());
        assert!(outcome.updated_branch.is_none());
        // The branch was NOT created.
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["rev-parse", "--verify", "--quiet", "refs/heads/dry-target"])
            .status()
            .unwrap();
        assert!(!out.success(), "dry-run must not create the target branch");
    }

    #[tokio::test]
    async fn rewrite_refuses_existing_into_branch() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
        run(dir.path(), &["branch", "already-exists"]);
        write(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        let preview = mk_preview(
            &intro,
            &intro,
            vec![(
                intro.clone(),
                "plan: foo".into(),
                RewriteDisposition::Drop,
                false,
                vec![],
            )],
        );
        let err = super::run(RewriteOpts {
            repo: dir.path(),
            preview: &preview,
            into_branch: Some("already-exists"),
            dry: false,
        })
        .await
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("already-exists"), "got: {msg}");
        assert!(msg.contains("already exists"), "got: {msg}");
    }

    #[tokio::test]
    async fn rewrite_refuses_non_linear_range() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let seed = commit(dir.path(), "seed");
        let preview = RewritePreviewResponse {
            plan_id: "trinity/foo.md".into(),
            plan_stem: PlanKey::parse("foo").unwrap(),
            intro_sha: Some(CommitSha::parse(&seed).unwrap()),
            head_sha: CommitSha::parse(&seed).unwrap(),
            linear: false,
            commits: Vec::new(),
        };
        let err = super::run(RewriteOpts {
            repo: dir.path(),
            preview: &preview,
            into_branch: Some("x"),
            dry: false,
        })
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("merge commit"), "{err}");
    }
}
