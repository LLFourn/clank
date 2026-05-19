//! Shared history-rewriting engine for `trinity purge` and
//! `trinity finish --purge`. Consumes the daemon's typed
//! `RewritePreviewResponse` and applies it via git plumbing.
//!
//! The engine never re-derives Trinity attribution or classification —
//! the daemon's manifest IS the rewrite plan. This module is
//! pure side-effect-producing infrastructure (git plumbing,
//! ref updates, tree builds).

use std::path::Path;

use trinity_core::api::{RewriteCommit, RewriteDisposition};
use trinity_core::ids::CommitSha;

/// Engine inputs. Unpacked fields so both single-plan and all-plans
/// preview responses can feed the same engine. `dry == true` makes
/// the engine compute everything up to the first `commit-tree` /
/// `update-ref` call and then exit without producing any git
/// objects or moving refs.
#[derive(Debug)]
pub struct RewriteOpts<'a> {
    pub repo: &'a Path,
    /// Earliest commit in the rewrite range. `None` is treated as
    /// "nothing to rewrite" — the engine refuses; callers should
    /// short-circuit before invoking.
    pub intro_sha: Option<&'a CommitSha>,
    /// Branch tip the preview was computed against. Used as the
    /// expected-old-value for the conditional in-place update-ref.
    pub head_sha: &'a CommitSha,
    /// False if the range contains a merge commit. Engine refuses
    /// in that case.
    pub linear: bool,
    /// Per-commit manifest, in chronological order.
    pub commits: &'a [RewriteCommit],
    /// `None` rewrites the current branch in place; `Some(name)`
    /// writes the rewritten chain to a fresh branch. Refuses if the
    /// branch already exists.
    pub into_branch: Option<&'a str>,
    pub dry: bool,
    /// Permit rewriting a protected branch (`main`/`master`/etc.)
    /// in place. Ignored when `into_branch` is set.
    pub allow_rewrite_protected: bool,
    /// `Some(msg)` collapses every commit in the range into one
    /// commit on top of `intro_parent`, with the supplied message.
    /// Refuses if any commit in the range is marked `foreign`
    /// (squash can't selectively preserve interleaved foreign
    /// work). The resulting tree is HEAD's tree minus the union
    /// of all strip_paths from the manifest.
    pub squash: Option<&'a str>,
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
///
/// In `--dry` mode the engine prints the planned listing FIRST,
/// then reports any blockers as a footer — the operator wants the
/// commit-by-commit shape even when a blocker would prevent the
/// live run.
pub async fn run(opts: RewriteOpts<'_>) -> anyhow::Result<RewriteOutcome> {
    // Build the execution plan from inputs we always have (commits
    // + maybe-intro_parent). Pre-flight checks below decide whether
    // to enforce or just report.
    let intro_parent = if let Some(intro) = opts.intro_sha {
        parent_of(opts.repo, intro.as_str())?
    } else {
        None
    };
    let plan = build_plan(opts.commits, intro_parent.as_deref());

    let blockers = collect_blockers(&opts)?;

    // Squash adds one more blocker: foreign commits in the range.
    // Squash can't selectively preserve interleaved foreign work;
    // the operator must `--purge` first if they want to handle
    // them.
    let mut blockers = blockers;
    if opts.squash.is_some() {
        let foreign = plan.steps.iter().filter(|s| s.foreign).count();
        if foreign > 0 {
            blockers.push(format!(
                "--squash refuses {foreign} foreign commit(s) in the rewrite \
                 range. Use `trinity purge` without `--squash` first, then \
                 retry."
            ));
        }
    }

    if opts.dry {
        print_rebase_todo(&opts, &plan, &blockers);
        return Ok(RewriteOutcome::default());
    }

    if let Some(first) = blockers.first() {
        // Live run: bail on the first blocker with the same
        // message --dry would have reported.
        anyhow::bail!("{first}");
    }
    // Re-prove intro exists since the live path uses it for the
    // conditional ref update — the blockers list already caught
    // the None case, this re-check is just for the type system.
    let intro = opts
        .intro_sha
        .expect("blockers would have caught a missing intro");
    let _ = intro;

    let new_tip = if let Some(message) = opts.squash {
        apply_squash(
            opts.repo,
            opts.head_sha,
            &plan,
            intro_parent.as_deref(),
            message,
        )
        .await?
    } else {
        apply_plan(opts.repo, &plan, intro_parent.as_deref()).await?
    };
    let updated_branch = match opts.into_branch {
        Some(name) => {
            // Atomic "must not already exist": all-zero old value
            // tells git to refuse if the ref exists. Closes the
            // race between `branch_exists` and `update-ref`.
            git_run(
                opts.repo,
                &[
                    "update-ref",
                    &format!("refs/heads/{name}"),
                    &new_tip,
                    "0000000000000000000000000000000000000000",
                ],
            )?;
            name.to_string()
        }
        None => {
            let current = current_branch(opts.repo)?;
            // Conditional update: expected old value is the head we
            // previewed against. If the branch moved between preview
            // and now, refuse — we'd be silently throwing away
            // commits that arrived after preview.
            git_run(
                opts.repo,
                &[
                    "update-ref",
                    &format!("refs/heads/{current}"),
                    &new_tip,
                    opts.head_sha.as_str(),
                ],
            )
            .map_err(|e| {
                anyhow::anyhow!(
                    "branch `{current}` moved between preview and rewrite \
                     (expected {}); aborting to avoid losing commits — re-run \
                     to refresh the preview. underlying: {e}",
                    short(opts.head_sha.as_str()),
                )
            })?;
            // Re-sync the worktree to the new tip.
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

/// Pre-flight diagnostics. Live run bails on the first; `--dry`
/// reports the full list as a footer after the listing.
fn collect_blockers(opts: &RewriteOpts<'_>) -> anyhow::Result<Vec<String>> {
    let mut blockers = Vec::new();
    if !opts.linear {
        blockers.push(
            "range contains a merge commit; refusing to rewrite (merge-tree \
             rewriting is out of scope)"
                .to_string(),
        );
    }
    if opts.intro_sha.is_none() {
        blockers.push("rewrite range is empty; nothing to do".to_string());
    }
    if working_tree_dirty(opts.repo)? {
        blockers.push("working tree dirty; commit or stash first".to_string());
    }
    if let Some(branch) = opts.into_branch
        && branch_exists(opts.repo, branch)?
    {
        blockers.push(format!(
            "branch `{branch}` already exists; refusing to overwrite. \
             Pick a different name or delete it first."
        ));
    }
    // Protected-branch refusal — only relevant for in-place
    // rewrites; --into-branch never touches the protected ref.
    if opts.into_branch.is_none() && !opts.allow_rewrite_protected {
        let current = current_branch(opts.repo)?;
        if is_protected_branch(opts.repo, &current)? {
            blockers.push(format!(
                "refusing to rewrite protected branch `{current}` in place. \
                 Pass `--allow-rewrite-protected` to override, or use \
                 `--into-branch <name>` to write the rewritten history to a \
                 fresh branch."
            ));
        }
    }
    Ok(blockers)
}

/// True if `branch` is `main`, `master`, or matched by
/// `branch.<name>.protect = true` in the repo's git config. The
/// engine refuses to rewrite protected branches in place without
/// `--allow-rewrite-protected`.
fn is_protected_branch(repo: &Path, branch: &str) -> anyhow::Result<bool> {
    if matches!(branch, "main" | "master") {
        return Ok(true);
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["config", "--get", &format!("branch.{branch}.protect")])
        .output()?;
    if !output.status.success() {
        return Ok(false);
    }
    let val = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(matches!(val.as_str(), "true" | "1" | "yes" | "on"))
}

/// Emit a `git rebase --interactive` todo list. The operator can
/// pipe it to git via `GIT_SEQUENCE_EDITOR='cp <file>' git rebase
/// --interactive --keep-empty <intro>^`, or just read it as a
/// preview. When ANY blocker is present every todo command is
/// commented out so the file isn't pipeable unchanged — `git`
/// ignores `#` lines, so a blocked output that left commands
/// uncommented would let the operator execute a rewrite the live
/// command would refuse.
fn print_rebase_todo(opts: &RewriteOpts<'_>, plan: &ExecutionPlan, blockers: &[String]) {
    let cmt = if blockers.is_empty() { "" } else { "# " };

    println!("# trinity rewrite preview");
    if let Some(intro) = opts.intro_sha {
        println!(
            "# range: {}..{} ({} commits in rewrite range)",
            short(intro.as_str()),
            short(opts.head_sha.as_str()),
            plan.steps.len(),
        );
    } else {
        println!("# range: (empty — no .trinity/ history)");
    }
    println!(
        "# starting parent: {}",
        plan.intro_parent.as_deref().unwrap_or("(root)"),
    );
    let target = match opts.into_branch {
        Some(b) => format!("a NEW branch `{b}` (current branch untouched)"),
        None => "the CURRENT branch (in place)".into(),
    };
    println!("# target: {target}");
    println!("#");
    if !blockers.is_empty() {
        println!("# BLOCKERS (live run would refuse):");
        for b in blockers {
            println!("#   - {b}");
        }
        println!("#");
        println!("# Every todo command below is commented out because of");
        println!("# the blockers above. Address them, re-run `--dry`,");
        println!("# and only then pipe to git.");
        println!("#");
    } else {
        println!("# Run with:");
        println!("#   GIT_SEQUENCE_EDITOR='cp <this-file>' \\");
        let intro_anchor = opts
            .intro_sha
            .map(|s| short(s.as_str()))
            .unwrap_or_else(|| "<intro>".into());
        println!("#     git rebase --interactive --keep-empty {intro_anchor}^");
        println!("#");
        println!("# Caveats:");
        println!("#   - `--into-branch` isn't supported by git rebase. To");
        println!("#     preview on a side branch: `git checkout -b scratch`");
        println!("#     before running the rebase.");
        println!("#   - No `<expected-old>` ref-update guard like the live");
        println!("#     engine. Don't race other branch updates.");
        println!("#   - `--keep-empty` is required so commits that become");
        println!("#     empty after strip stay visible for review.");
        println!("#");
    }

    let foreign_count = plan.steps.iter().filter(|s| s.foreign).count();
    if foreign_count > 0 {
        println!("# {foreign_count} commit(s) in the range are foreign to this plan");
        println!("# (they appear here so the parent chain is contiguous).");
        println!("#");
    }

    for step in &plan.steps {
        let foreign_tag = if step.foreign { " [foreign]" } else { "" };
        match step.disposition {
            RewriteDisposition::Drop => {
                println!(
                    "{cmt}drop  {} {}{}",
                    short(&step.sha),
                    step.subject,
                    foreign_tag
                );
            }
            RewriteDisposition::KeepVerbatim => {
                println!(
                    "{cmt}pick  {} {}{}",
                    short(&step.sha),
                    step.subject,
                    foreign_tag
                );
            }
            RewriteDisposition::Rewrite => {
                println!(
                    "{cmt}edit  {} {}{}",
                    short(&step.sha),
                    step.subject,
                    foreign_tag
                );
                let joined = step
                    .strip_paths
                    .iter()
                    .map(|p| shell_quote(p))
                    .collect::<Vec<_>>()
                    .join(" ");
                println!("{cmt}# strip: {}", step.strip_paths.join(", "));
                println!("{cmt}# run: git rm --cached {joined} && \\");
                println!("{cmt}#      git commit --amend --no-edit --allow-empty && \\");
                println!("{cmt}#      git rebase --continue");
            }
        }
    }
}

/// POSIX-style single-quote escaping for paths that might contain
/// spaces or shell metacharacters. Operators copy-paste these into
/// a shell so they need to be safe.
fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
    {
        s.to_string()
    } else {
        // Wrap in single quotes; escape any existing single quotes
        // with the standard `'\''` dance.
        let escaped = s.replace('\'', r"'\''");
        format!("'{escaped}'")
    }
}

/// Walk `plan.steps` in order, producing a new tip SHA. When every
/// step is `Drop`, the branch should move to `intro_parent` — that's
/// "plan abandoned mid-flight, leave no trace." When `intro_parent`
/// is `None` (intro is the root commit), there's no valid SHA to
/// point the branch at, so we error with a clear message: the
/// operator must keep at least one commit or delete the branch
/// outright.
async fn apply_plan(
    repo: &Path,
    plan: &ExecutionPlan,
    intro_parent: Option<&str>,
) -> anyhow::Result<String> {
    let mut parent: Option<String> = plan.intro_parent.clone();
    let mut produced_any = false;
    for step in &plan.steps {
        match step.disposition {
            RewriteDisposition::Drop => {
                // Parent chain hops over this commit.
            }
            RewriteDisposition::KeepVerbatim => {
                let tree = git_capture(repo, &["rev-parse", &format!("{}^{{tree}}", step.sha)])?;
                let new_sha =
                    commit_tree_preserving_meta(repo, &step.sha, &tree, parent.as_deref())?;
                parent = Some(new_sha);
                produced_any = true;
            }
            RewriteDisposition::Rewrite => {
                let new_tree = build_stripped_tree(repo, &step.sha, &step.strip_paths)?;
                let new_sha =
                    commit_tree_preserving_meta(repo, &step.sha, &new_tree, parent.as_deref())?;
                parent = Some(new_sha);
                produced_any = true;
            }
        }
    }
    if !produced_any {
        match intro_parent {
            Some(p) => Ok(p.to_string()),
            None => anyhow::bail!(
                "every commit in range would be dropped AND the plan's intro is a root commit — \
                 cannot point a branch at 'nothing'. Use `git branch -D` to delete the branch \
                 outright if that's what you want.",
            ),
        }
    } else {
        parent.ok_or_else(|| anyhow::anyhow!("rewrite produced commits but lost the tip"))
    }
}

/// Collapse the rewrite range into ONE commit on top of
/// `intro_parent`. Tree = HEAD's tree minus the union of all
/// strip_paths across the manifest. Foreign-commit refusal lives
/// in `run()`'s blocker check; this function assumes the range
/// is clean.
async fn apply_squash(
    repo: &Path,
    head_sha: &CommitSha,
    plan: &ExecutionPlan,
    intro_parent: Option<&str>,
    message: &str,
) -> anyhow::Result<String> {
    let mut strip_union: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for step in &plan.steps {
        for p in &step.strip_paths {
            strip_union.insert(p.as_str());
        }
    }
    let strip: Vec<String> = strip_union.iter().map(|s| s.to_string()).collect();
    // Build the squashed tree from HEAD's tree (the union of
    // everything the range produced) with the strip paths removed.
    // Paths in the strip set but absent from HEAD's tree are no-ops
    // for `update-index --force-remove` — added-then-deleted in the
    // range, already gone.
    let head_str = head_sha.as_str();
    let new_tree = build_stripped_tree_from_sha(repo, head_str, &strip)?;
    // Author/timestamp: prefer HEAD's so the squashed commit
    // doesn't look like a fresh authorship event from now.
    // Committer-side fields will refresh.
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(repo);
    let author = git_capture(repo, &["show", "-s", "--format=%an <%ae>", head_str])?;
    let author_date = git_capture(repo, &["show", "-s", "--format=%aI", head_str])?;
    if let Some((name, email)) = parse_author(author.trim()) {
        cmd.env("GIT_AUTHOR_NAME", name);
        cmd.env("GIT_AUTHOR_EMAIL", email);
    }
    cmd.env("GIT_AUTHOR_DATE", author_date.trim());
    cmd.args(["commit-tree", &new_tree]);
    if let Some(p) = intro_parent {
        cmd.args(["-p", p]);
    }
    cmd.arg("-m").arg(message);
    let output = cmd.output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git commit-tree failed for squash: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Like `build_stripped_tree` but takes a sha that we look up
/// `^{tree}` for. Used by squash where we work from HEAD's tree
/// directly.
fn build_stripped_tree_from_sha(
    repo: &Path,
    sha: &str,
    strip_paths: &[String],
) -> anyhow::Result<String> {
    let scratch_dir = repo.join(".git").join("trinity-rewrite");
    std::fs::create_dir_all(&scratch_dir)?;
    let unique = format!(
        "squash-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    let index_path = scratch_dir.join(unique);
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
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("purged"),
            dry: false,
            allow_rewrite_protected: false,
            squash: None,
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
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("dry-target"),
            dry: true,
            allow_rewrite_protected: false,
            squash: None,
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
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("already-exists"),
            dry: false,
            allow_rewrite_protected: false,
            squash: None,
        })
        .await
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("already-exists"), "got: {msg}");
        assert!(msg.contains("already exists"), "got: {msg}");
    }

    #[tokio::test]
    async fn rewrite_all_drop_points_branch_at_intro_parent() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let seed = commit(dir.path(), "seed");
        write(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        write(dir.path(), ".trinity/plans/foo.md", "# foo v2\n");
        let revision = commit(dir.path(), "plan: foo v2");

        let preview = mk_preview(
            &intro,
            &revision,
            vec![
                (
                    intro.clone(),
                    "plan: foo".into(),
                    RewriteDisposition::Drop,
                    false,
                    vec![],
                ),
                (
                    revision.clone(),
                    "plan: foo v2".into(),
                    RewriteDisposition::Drop,
                    false,
                    vec![],
                ),
            ],
        );
        let outcome = super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("purged"),
            dry: false,
            allow_rewrite_protected: false,
            squash: None,
        })
        .await
        .unwrap();
        assert_eq!(outcome.new_tip.as_deref(), Some(seed.as_str()));
        let chain = rev_list(dir.path(), "purged");
        assert_eq!(chain, vec![seed]);
    }

    #[tokio::test]
    async fn rewrite_conditional_in_place_update_refuses_if_branch_moved() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
        write(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");

        // Simulate: preview was fetched against `intro`, then a new
        // commit landed.
        write(dir.path(), "src/main.rs", "fn main() {}\n");
        let raced_commit = commit(dir.path(), "raced commit after preview");

        let preview = mk_preview(
            &intro,
            &intro, // stale head_sha
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
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: None, // in-place — triggers the conditional update
            dry: false,
            // The test's repo uses the default `main` branch, which the new
            // protected-branch check refuses. Override here — the test's
            // assertion is about the in-place race, not protected-branch
            // policy.
            allow_rewrite_protected: true,
            squash: None,
        })
        .await
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("moved between preview"), "got: {msg}");

        // Branch tip is still the raced commit; the engine did not
        // clobber it.
        let chain = rev_list(dir.path(), "main");
        assert_eq!(chain.last().unwrap(), &raced_commit);
    }

    /// End-to-end single-plan purge: codex's specific request.
    /// `trinity purge foo --into-branch scrubbed` on
    /// `seed → plan → code` must produce a `scrubbed` branch that
    /// contains `src/main.rs` and does NOT contain
    /// `.trinity/plans/foo.md`. The previous diff-touch
    /// classification would have marked `code` as KeepVerbatim and
    /// leaked the inherited plan file.
    #[tokio::test]
    async fn rewrite_single_plan_strips_inherited_plan_file() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
        write(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        write(dir.path(), "src/main.rs", "fn main() {}\n");
        let code = commit(dir.path(), "later code");

        // Manifest as the (corrected) single-plan endpoint would
        // emit it: intro Drop, code Rewrite with strip_paths=[foo.md].
        let preview = mk_preview(
            &intro,
            &code,
            vec![
                (
                    intro.clone(),
                    "plan: foo".into(),
                    RewriteDisposition::Drop,
                    false,
                    vec![],
                ),
                (
                    code.clone(),
                    "later code".into(),
                    RewriteDisposition::Rewrite,
                    false,
                    vec![".trinity/plans/foo.md".into()],
                ),
            ],
        );
        super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("scrubbed"),
            dry: false,
            allow_rewrite_protected: false,
            squash: None,
        })
        .await
        .unwrap();
        assert!(
            !tree_has(dir.path(), "scrubbed", ".trinity/plans/foo.md"),
            "inherited plan file leaked into scrubbed branch"
        );
        assert!(tree_has(dir.path(), "scrubbed", "src/main.rs"));
    }

    /// End-to-end: simulate an all-plans purge across an intro
    /// commit + a pure-code commit. Asserts that with strip_paths
    /// populated on the KeepVerbatim-equivalent step, the
    /// rewritten branch's final tree does NOT contain the plan
    /// file even though that commit's diff didn't touch it.
    #[tokio::test]
    async fn rewrite_strips_inherited_trinity_from_later_pure_code() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
        write(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        write(dir.path(), "src/main.rs", "fn main() {}\n");
        let code = commit(dir.path(), "later code");

        // Manifest mirrors what api_rewrite_preview_all would
        // emit with tree-based classification: intro Drops, the
        // pure-code commit Rewrites with the inherited
        // .trinity/plans/foo.md in strip_paths.
        let preview = mk_preview(
            &intro,
            &code,
            vec![
                (
                    intro.clone(),
                    "plan: foo".into(),
                    RewriteDisposition::Drop,
                    false,
                    vec![],
                ),
                (
                    code.clone(),
                    "later code".into(),
                    RewriteDisposition::Rewrite,
                    false,
                    vec![".trinity/plans/foo.md".into()],
                ),
            ],
        );
        super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("scrubbed"),
            dry: false,
            allow_rewrite_protected: false,
            squash: None,
        })
        .await
        .unwrap();

        // The scrubbed branch's tip must contain src/main.rs but
        // NOT .trinity/plans/foo.md.
        assert!(
            !tree_has(dir.path(), "scrubbed", ".trinity/plans/foo.md"),
            "plan file leaked into rewritten branch"
        );
        assert!(
            tree_has(dir.path(), "scrubbed", "src/main.rs"),
            "code file lost from rewritten branch"
        );
        assert!(
            tree_has(dir.path(), "scrubbed", "README.md"),
            "seed file lost from rewritten branch"
        );
    }

    #[test]
    fn shell_quote_handles_metachars() {
        assert_eq!(shell_quote("simple/path.md"), "simple/path.md");
        assert_eq!(shell_quote("a b.md"), "'a b.md'");
        assert_eq!(shell_quote("isn't.md"), r"'isn'\''t.md'");
    }

    #[tokio::test]
    async fn rewrite_squash_collapses_range_into_one_commit() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
        write(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        write(dir.path(), ".trinity/plans/foo.md", "# foo v2\n");
        write(dir.path(), "src/lib.rs", "// code v1\n");
        let _mixed = commit(dir.path(), "plan revision + code");
        write(dir.path(), "src/main.rs", "fn main() {}\n");
        let code = commit(dir.path(), "more code");

        let preview = mk_preview(
            &intro,
            &code,
            vec![
                (
                    intro.clone(),
                    "plan: foo".into(),
                    RewriteDisposition::Drop,
                    false,
                    vec![],
                ),
                (
                    _mixed.clone(),
                    "plan revision + code".into(),
                    RewriteDisposition::Rewrite,
                    false,
                    vec![".trinity/plans/foo.md".into()],
                ),
                (
                    code.clone(),
                    "more code".into(),
                    RewriteDisposition::Rewrite,
                    false,
                    vec![".trinity/plans/foo.md".into()],
                ),
            ],
        );
        super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("squashed"),
            dry: false,
            allow_rewrite_protected: false,
            squash: Some("Implement foo"),
        })
        .await
        .unwrap();

        // The squashed branch should be: seed → ONE squash commit
        // containing src/lib.rs + src/main.rs + README.md but no
        // .trinity/plans/foo.md.
        let chain = rev_list(dir.path(), "squashed");
        assert_eq!(chain.len(), 2, "seed + one squash commit; got {chain:?}");
        let squash_tip = chain.last().unwrap();
        assert_eq!(show_subject(dir.path(), squash_tip), "Implement foo");
        assert!(tree_has(dir.path(), squash_tip, "src/lib.rs"));
        assert!(tree_has(dir.path(), squash_tip, "src/main.rs"));
        assert!(tree_has(dir.path(), squash_tip, "README.md"));
        assert!(!tree_has(dir.path(), squash_tip, ".trinity/plans/foo.md"));
    }

    #[tokio::test]
    async fn rewrite_squash_refuses_foreign_commit_in_range() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
        write(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        let preview = mk_preview(
            &intro,
            &intro,
            vec![(
                intro.clone(),
                "plan: foo".into(),
                RewriteDisposition::Drop,
                true, // foreign
                vec![],
            )],
        );
        let err = super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("squashed"),
            dry: false,
            allow_rewrite_protected: false,
            squash: Some("collapse"),
        })
        .await
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("foreign"), "got: {msg}");
    }

    #[tokio::test]
    async fn rewrite_refuses_protected_branch_in_place() {
        // The default `main` branch is protected; in-place
        // rewrites refuse without --allow-rewrite-protected.
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
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
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: None, // in-place
            dry: false,
            allow_rewrite_protected: false,
            squash: None,
        })
        .await
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("protected branch"),
            "expected protected-branch refusal, got: {msg}"
        );
        assert!(msg.contains("main"), "msg should name the branch: {msg}");
    }

    #[tokio::test]
    async fn rewrite_protected_branch_bypassed_by_into_branch() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
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
        // --into-branch bypasses protected check; this should succeed.
        super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("scrubbed"),
            dry: false,
            allow_rewrite_protected: false,
            squash: None,
        })
        .await
        .unwrap();
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
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("x"),
            dry: false,
            allow_rewrite_protected: false,
            squash: None,
        })
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("merge commit"), "{err}");
    }
}
