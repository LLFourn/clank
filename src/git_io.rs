//! Git introspection for the filesystem-truth model. Narrowly-scoped
//! reads only — never writes. Functions here run `git` as a subprocess
//! and parse the output.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::attribution::{CommitChanges, PlanTouch};
use crate::disk_format::{parse_feedback_path, plan_path_is_done};
use crate::disk_snapshot::{DiskSnapshot, FeedbackBlob, HistoryEntry, PlanFileBlob};
use crate::lifecycle::{CommitSha, PlanKey, PlanPath};
use crate::repo_state::PlanTouchKind;

#[derive(Debug, thiserror::Error)]
pub enum GitIoError {
    #[error("git command failed to spawn: {0}")]
    Spawn(String),
    #[error("git {context}: exit {code:?}: {stderr}")]
    NonZero {
        context: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("git output parse failure ({context}): {detail}")]
    Parse { context: String, detail: String },
}

async fn run(repo: &Path, args: &[&str]) -> Result<std::process::Output, GitIoError> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await
        .map_err(|e| GitIoError::Spawn(format!("{e}")))
}

async fn run_ok(repo: &Path, args: &[&str]) -> Result<String, GitIoError> {
    let output = run(repo, args).await?;
    if !output.status.success() {
        return Err(GitIoError::NonZero {
            context: format!("git {}", args.join(" ")),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

/// Like `run_ok` but preserves trailing newlines. Used for blob reads
/// where the exact body matters for hashing.
async fn run_ok_raw(repo: &Path, args: &[&str]) -> Result<String, GitIoError> {
    let output = run(repo, args).await?;
    if !output.status.success() {
        return Err(GitIoError::NonZero {
            context: format!("git {}", args.join(" ")),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `git rev-parse HEAD`. Returns `Ok(None)` for an empty repo (no commits).
pub async fn rev_parse_head(repo: &Path) -> Result<Option<CommitSha>, GitIoError> {
    let output = run(repo, &["rev-parse", "HEAD"]).await?;
    if !output.status.success() {
        // Most likely: empty repo. Treat as None.
        return Ok(None);
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        Ok(None)
    } else {
        Ok(Some(CommitSha::from(s)))
    }
}

/// A committed plan file under `.trinity/plans/` (or `.trinity/plans/done/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    /// Path relative to the repo root.
    pub path: PathBuf,
    pub blob_sha: String,
}

/// `git ls-tree -r HEAD -- .trinity/plans/` parsed into structured entries.
///
/// Returns an empty vec if the path doesn't exist in HEAD.
pub async fn ls_tree_plans(repo: &Path, head: &CommitSha) -> Result<Vec<PlanEntry>, GitIoError> {
    let output = run(
        repo,
        &["ls-tree", "-r", "--", head.as_str(), ".trinity/plans/"],
    )
    .await?;
    // Path-not-in-tree is `exit 0` with empty output, but older git versions
    // return non-zero. Treat any failure here as "no entries."
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();
    for line in stdout.lines() {
        // Format: <mode> <type> <sha>\t<path>
        let (head_part, path) = match line.split_once('\t') {
            Some(p) => p,
            None => continue,
        };
        let mut tokens = head_part.split_ascii_whitespace();
        let _mode = tokens.next();
        let kind = tokens.next();
        let sha = tokens.next();
        if kind != Some("blob") {
            continue;
        }
        let sha = match sha {
            Some(s) => s.to_string(),
            None => continue,
        };
        entries.push(PlanEntry {
            path: PathBuf::from(path),
            blob_sha: sha,
        });
    }
    Ok(entries)
}

/// `git show HEAD:<path>` — return the blob content at the given ref.
/// Preserves trailing whitespace (newlines matter for hashing).
pub async fn show_blob(
    repo: &Path,
    rev: &CommitSha,
    rel_path: &Path,
) -> Result<String, GitIoError> {
    let spec = format!("{}:{}", rev.as_str(), rel_path.display());
    run_ok_raw(repo, &["show", &spec]).await
}

/// `git show <sha>` — return the full commit patch (header + diff) as text.
/// Used by the commit-diff route to render impl commits.
/// Patch text from `git diff <from> <to> -- <path>` (unified diff form,
/// parseable by `diff_parser::parse_diff`). Used by the plan-rev-vs-rev
/// endpoint to compare two snapshots of one file.
pub async fn diff_two_blobs(
    repo: &Path,
    from: &CommitSha,
    to: &CommitSha,
    path: &Path,
) -> Result<String, GitIoError> {
    let path_str = path.to_string_lossy();
    run_ok_raw(
        repo,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            from.as_str(),
            to.as_str(),
            "--",
            &path_str,
        ],
    )
    .await
}

pub async fn show_commit(repo: &Path, sha: &CommitSha) -> Result<String, GitIoError> {
    run_ok_raw(repo, &["show", "--no-color", sha.as_str()]).await
}

/// `git rev-parse <sha>^` — first parent of the given commit. Returns
/// `Ok(None)` for the root commit (no parent).
pub async fn parent_of(repo: &Path, sha: &CommitSha) -> Result<Option<CommitSha>, GitIoError> {
    let spec = format!("{}^", sha.as_str());
    let output = run(repo, &["rev-parse", "--verify", &spec]).await?;
    if !output.status.success() {
        return Ok(None);
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        Ok(None)
    } else {
        Ok(Some(CommitSha::from(s)))
    }
}

/// `git log --diff-filter=A --follow --format=%H -- <path>` — the commit
/// that first introduced `path` along the follow chain. Returns
/// `Ok(None)` if no such commit exists (e.g. the path was never added,
/// which shouldn't happen for a path returned by `ls_tree_plans`).
pub async fn first_added_commit(
    repo: &Path,
    rel_path: &Path,
) -> Result<Option<CommitSha>, GitIoError> {
    let path_str = rel_path.to_str().ok_or_else(|| GitIoError::Parse {
        context: "first_added_commit".into(),
        detail: format!("non-utf8 path: {}", rel_path.display()),
    })?;
    let s = run_ok(
        repo,
        &[
            "log",
            "--diff-filter=A",
            "--follow",
            "--format=%H",
            "--",
            path_str,
        ],
    )
    .await?;
    // `--follow` may report multiple A commits across renames; the oldest
    // is the last line of `--format=%H` output.
    let oldest = s.lines().last().map(str::trim).filter(|s| !s.is_empty());
    Ok(oldest.map(|s| CommitSha::from(s.to_string())))
}

/// `git log --first-parent --reverse --format=%H` — all commits along
/// the first-parent chain from the root up to HEAD, oldest first. Used
/// for the attribution walk.
///
/// We deliberately don't try to bound by an `<intro>..HEAD` range: with
/// multiple sessions each having their own intro, identifying the
/// topologically earliest plan_intro requires a separate query. Trinity
/// repos are small enough that walking from the root is cheap and avoids
/// a correctness footgun.
pub async fn first_parent_commits(repo: &Path) -> Result<Vec<CommitSha>, GitIoError> {
    let stdout = run_ok(repo, &["log", "--first-parent", "--reverse", "--format=%H"]).await?;
    Ok(stdout
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| CommitSha::from(l.to_string()))
        .collect())
}

/// `git diff-tree -r --name-status -M --no-commit-id <sha>` parsed into a
/// `CommitChanges`. Renames are detected as `R<score>` entries with both
/// old and new paths.
///
/// For the root commit (no parent), uses `--root` form to enumerate its
/// added files.
pub async fn diff_tree_changes(repo: &Path, sha: &CommitSha) -> Result<CommitChanges, GitIoError> {
    let output = run(
        repo,
        &[
            "diff-tree",
            "-r",
            "--root",
            "--no-commit-id",
            "--name-status",
            "-M",
            sha.as_str(),
        ],
    )
    .await?;
    if !output.status.success() {
        return Err(GitIoError::NonZero {
            context: format!("diff-tree {}", sha.as_str()),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    parse_diff_tree(&String::from_utf8_lossy(&output.stdout))
}

fn parse_diff_tree(stdout: &str) -> Result<CommitChanges, GitIoError> {
    let mut plan_touches: Vec<PlanTouch> = Vec::new();
    let mut has_non_plan_code_changes = false;

    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(4, '\t');
        let status = parts.next().ok_or_else(|| GitIoError::Parse {
            context: "diff-tree".into(),
            detail: format!("empty status: {line}"),
        })?;
        let first_path = parts.next().ok_or_else(|| GitIoError::Parse {
            context: "diff-tree".into(),
            detail: format!("missing path: {line}"),
        })?;
        let second_path = parts.next();

        // Rename / Copy entries: `R<score>` or `C<score>` with two paths.
        let is_rename = status.starts_with('R') || status.starts_with('C');
        let new_path = if is_rename {
            second_path.unwrap_or(first_path)
        } else {
            first_path
        };
        let old_path = if is_rename { Some(first_path) } else { None };

        let new_rel = PathBuf::from(new_path);
        let new_is_plan = is_plan_path(&new_rel);
        let old_is_plan = old_path
            .map(|p| is_plan_path(&PathBuf::from(p)))
            .unwrap_or(false);

        if new_is_plan || old_is_plan {
            // Pick the path for session-id derivation: new path if it's
            // still in `plans/`, otherwise the old path (rename-out case).
            let old_plan_path = old_path.map(PathBuf::from);
            let touch_path: &Path = if new_is_plan {
                &new_rel
            } else {
                old_plan_path.as_deref().unwrap_or(&new_rel)
            };
            let plan_key = match PlanKey::from_path(touch_path) {
                Some(id) => id,
                None => continue, // unparseable plan name; skip
            };
            let kind = match status.chars().next().unwrap_or(' ') {
                'A' => {
                    // First-time add. We can't tell intro-vs-recreate at this
                    // layer (need history). Caller can cross-check against
                    // first_added_commit; for classification today, treat any
                    // A as Intro and let the caller refine if desired.
                    PlanTouchKind::Intro
                }
                'R' | 'C' => {
                    // Rename: classify based on whether it crosses the
                    // active/done boundary.
                    let was_done = old_path
                        .map(|p| plan_path_is_done(&PathBuf::from(p)))
                        .unwrap_or(false);
                    let is_done = plan_path_is_done(&new_rel);
                    if was_done != is_done {
                        PlanTouchKind::DoneMove
                    } else {
                        PlanTouchKind::Revision
                    }
                }
                _ => PlanTouchKind::Revision,
            };
            plan_touches.push(PlanTouch {
                session: plan_key,
                kind,
            });
        } else {
            // Non-plan path. Check if it's outside `.trinity/` for code-change.
            if !new_rel.starts_with(".trinity") {
                has_non_plan_code_changes = true;
            }
        }
    }

    Ok(CommitChanges {
        plan_touches,
        has_non_plan_code_changes,
    })
}

/// Gather a `DiskSnapshot` for `repo_root`. This is the IO half of the
/// rebuild flow; `disk_snapshot::derive_state` consumes the result and
/// is the pure half.
///
/// Steps:
/// 1. `git rev-parse HEAD` (empty repo → snapshot with `head=None`).
/// 2. `git ls-tree -r HEAD -- .trinity/plans/` for session discovery;
///    for each blob, fetch its body via `git show HEAD:<path>`, its
///    plan_intro via `--diff-filter=A --follow`, and the intro's
///    first-parent via `rev-parse <intro>^`.
/// 3. `git log --first-parent --reverse --format=%H` for the commit
///    chain; for each commit, `git diff-tree -r --name-status -M`
///    → `CommitChanges`.
/// 4. Walk `<repo>/.trinity/feedback/` for working-tree feedback files.
pub async fn snapshot(repo_root: &Path) -> Result<DiskSnapshot, GitIoError> {
    let head = rev_parse_head(repo_root).await?;
    let Some(head) = head else {
        return Ok(DiskSnapshot::default());
    };

    // Plan files in HEAD.
    let entries = ls_tree_plans(repo_root, &head).await?;
    let mut plan_files = Vec::with_capacity(entries.len());
    for e in entries {
        let Some(plan_key) = PlanKey::from_path(&e.path) else {
            continue;
        };
        let body = show_blob(repo_root, &head, &e.path).await?;
        let Some(plan_intro) = first_added_commit(repo_root, &e.path).await? else {
            continue;
        };
        let plan_intro_parent = parent_of(repo_root, &plan_intro).await?;
        plan_files.push(PlanFileBlob {
            plan_key,
            plan_path: PlanPath::new(e.path),
            body,
            plan_intro,
            plan_intro_parent,
        });
    }

    // Commit history along the first-parent chain. Skip the walk
    // entirely if there are no sessions — there's nothing to attribute.
    let history = if plan_files.is_empty() {
        Vec::new()
    } else {
        let shas = first_parent_commits(repo_root).await?;
        let mut out = Vec::with_capacity(shas.len());
        for sha in shas {
            let changes = diff_tree_changes(repo_root, &sha).await?;
            out.push(HistoryEntry {
                commit: sha,
                changes,
            });
        }
        out
    };

    // Feedback files in the working tree.
    let feedback_files = collect_feedback_files(repo_root)?;

    Ok(DiskSnapshot {
        head: Some(head),
        plan_files,
        history,
        feedback_files,
    })
}

fn collect_feedback_files(repo_root: &Path) -> Result<Vec<FeedbackBlob>, GitIoError> {
    let feedback_root = repo_root.join(".trinity").join("feedback");
    if !feedback_root.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    walk_files(&feedback_root, 4, &mut paths).map_err(|e| GitIoError::Parse {
        context: "walk feedback dir".into(),
        detail: format!("{e}"),
    })?;
    let mut out = Vec::with_capacity(paths.len());
    for abs in paths {
        let Ok(rel) = abs.strip_prefix(&feedback_root) else {
            continue;
        };
        let Some(parsed) = parse_feedback_path(rel) else {
            continue;
        };
        let body = std::fs::read_to_string(&abs).map_err(|e| GitIoError::Parse {
            context: "read feedback file".into(),
            detail: format!("{}: {e}", abs.display()),
        })?;
        let created_at = file_mtime_unix_secs(&abs);
        out.push(FeedbackBlob {
            abs_path: abs,
            parsed,
            body,
            created_at,
        });
    }
    Ok(out)
}

/// File mtime as unix seconds. Returns 0 with a `tracing::warn` if the
/// metadata read or unix-epoch conversion fails — the UI treats 0 as
/// "no chronological hint" rather than erroring the whole request.
/// Shared by both the rebuild path (initial `collect_feedback_files`)
/// and the watcher-driven `runtime::upsert_*` callsites.
pub(crate) fn file_mtime_unix_secs(path: &Path) -> i64 {
    match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(t) => match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = ?err,
                    "feedback mtime before unix epoch; falling back to 0"
                );
                0
            }
        },
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = ?err,
                "feedback mtime read failed; falling back to 0"
            );
            0
        }
    }
}

fn walk_files(root: &Path, max_depth: usize, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    fn walk(dir: &Path, depth: usize, max: usize, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        if depth > max {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                walk(&path, depth + 1, max, out)?;
            } else if ft.is_file() {
                out.push(path);
            }
        }
        Ok(())
    }
    walk(root, 1, max_depth, out)
}

fn is_plan_path(rel: &Path) -> bool {
    // `.trinity/plans/<name>.md` or `.trinity/plans/done/<name>.md`.
    let mut comps = rel.components().filter_map(|c| match c {
        std::path::Component::Normal(s) => s.to_str(),
        _ => None,
    });
    if comps.next() != Some(".trinity") {
        return false;
    }
    if comps.next() != Some("plans") {
        return false;
    }
    // Either <name>.md (depth 3) or done/<name>.md (depth 4).
    let third = match comps.next() {
        Some(s) => s,
        None => return false,
    };
    if third == "done" {
        match comps.next() {
            Some(name) => comps.next().is_none() && name.ends_with(".md"),
            None => false,
        }
    } else {
        comps.next().is_none() && third.ends_with(".md")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_plan_path_active() {
        assert!(is_plan_path(&PathBuf::from(".trinity/plans/foo.md")));
    }

    #[test]
    fn is_plan_path_done() {
        assert!(is_plan_path(&PathBuf::from(".trinity/plans/done/foo.md")));
    }

    #[test]
    fn is_not_plan_path_feedback() {
        assert!(!is_plan_path(&PathBuf::from(
            ".trinity/feedback/foo/plan/alice.md"
        )));
    }

    #[test]
    fn is_not_plan_path_source() {
        assert!(!is_plan_path(&PathBuf::from("src/lib.rs")));
    }

    #[test]
    fn is_not_plan_path_too_deep() {
        assert!(!is_plan_path(&PathBuf::from(".trinity/plans/sub/foo.md")));
    }

    #[test]
    fn parse_diff_tree_single_plan_intro() {
        let stdout = "A\t.trinity/plans/foo.md\n";
        let changes = parse_diff_tree(stdout).unwrap();
        assert_eq!(changes.plan_touches.len(), 1);
        assert_eq!(changes.plan_touches[0].session.as_str(), "foo");
        assert!(matches!(changes.plan_touches[0].kind, PlanTouchKind::Intro));
        assert!(!changes.has_non_plan_code_changes);
    }

    #[test]
    fn parse_diff_tree_plan_revision_with_code() {
        let stdout = "M\t.trinity/plans/foo.md\nM\tsrc/lib.rs\n";
        let changes = parse_diff_tree(stdout).unwrap();
        assert_eq!(changes.plan_touches.len(), 1);
        assert!(matches!(
            changes.plan_touches[0].kind,
            PlanTouchKind::Revision
        ));
        assert!(changes.has_non_plan_code_changes);
    }

    #[test]
    fn parse_diff_tree_pure_code() {
        let stdout = "M\tsrc/foo.rs\nA\ttests/bar.rs\n";
        let changes = parse_diff_tree(stdout).unwrap();
        assert!(changes.plan_touches.is_empty());
        assert!(changes.has_non_plan_code_changes);
    }

    #[test]
    fn parse_diff_tree_done_move() {
        let stdout = "R100\t.trinity/plans/foo.md\t.trinity/plans/done/foo.md\n";
        let changes = parse_diff_tree(stdout).unwrap();
        assert_eq!(changes.plan_touches.len(), 1);
        assert!(matches!(
            changes.plan_touches[0].kind,
            PlanTouchKind::DoneMove
        ));
        assert_eq!(changes.plan_touches[0].session.as_str(), "foo");
    }

    #[test]
    fn parse_diff_tree_multi_plan_touch() {
        let stdout = "M\t.trinity/plans/foo.md\nA\t.trinity/plans/bar.md\nM\tsrc/lib.rs\n";
        let changes = parse_diff_tree(stdout).unwrap();
        assert_eq!(changes.plan_touches.len(), 2);
        assert!(changes.has_non_plan_code_changes);
    }

    #[test]
    fn parse_diff_tree_ignores_other_trinity_paths() {
        // Feedback files are gitignored in practice, but if any sneak in,
        // they should count as neither plan nor code change.
        let stdout = "A\t.trinity/feedback/foo/plan/alice.md\n";
        let changes = parse_diff_tree(stdout).unwrap();
        assert!(changes.plan_touches.is_empty());
        assert!(!changes.has_non_plan_code_changes);
    }
}
