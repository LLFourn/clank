//! Git introspection for the filesystem-truth model. Narrowly-scoped
//! reads only — never writes. Functions here run `git` as a subprocess
//! and parse the output.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::attribution::{CommitChanges, FinalizeChange, FinalizeChangeKind, PlanTouch};
use crate::disk_format::{parse_feedback_path, parse_finalize_path};
use crate::disk_snapshot::{CommitEvent, DiskSnapshot, FeedbackBlob};
use crate::lifecycle::{CommitSha, PlanKey};
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

fn parse_sha(context: &str, s: &str) -> Result<CommitSha, GitIoError> {
    CommitSha::parse(s).map_err(|e| GitIoError::Parse {
        context: context.to_string(),
        detail: e.to_string(),
    })
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
        Ok(Some(parse_sha("rev-parse HEAD", &s)?))
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
/// Patch text from `git diff <from>:<from_path> <to>:<to_path>` (unified
/// diff form, parseable by `diff_parser::parse_diff`). The two paths
/// differ when the plan file moved between active↔done somewhere between
/// `from` and `to`; for revisions on the same side they're equal.
pub async fn diff_two_blobs(
    repo: &Path,
    from: &CommitSha,
    from_path: &Path,
    to: &CommitSha,
    to_path: &Path,
) -> Result<String, GitIoError> {
    let from_spec = format!("{}:{}", from.as_str(), from_path.display());
    let to_spec = format!("{}:{}", to.as_str(), to_path.display());
    run_ok_raw(
        repo,
        &["diff", "--no-color", "--no-ext-diff", &from_spec, &to_spec],
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
        Ok(Some(parse_sha("parent_of", &s)?))
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
    oldest
        .map(|s| parse_sha("first_added_commit", s))
        .transpose()
}

/// `git log --first-parent --reverse --format=%H` — all commits along
/// the first-parent chain from the root up to HEAD, oldest first. Used
/// for the attribution walk. Each entry carries the author timestamp
/// (unix seconds) and the commit subject (first line), batched into one
/// `git log` invocation so per-rebuild git overhead stays bounded.
///
/// We deliberately don't try to bound by an `<intro>..HEAD` range: with
/// multiple sessions each having their own intro, identifying the
/// topologically earliest plan_intro requires a separate query. Trinity
/// repos are small enough that walking from the root is cheap and avoids
/// a correctness footgun.
pub async fn first_parent_commits(repo: &Path) -> Result<Vec<CommitMeta>, GitIoError> {
    let stdout = run_ok(
        repo,
        &[
            "log",
            "--first-parent",
            "--reverse",
            "--format=%H%x00%at%x00%s",
        ],
    )
    .await?;
    let mut out = Vec::new();
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, '\0');
        let sha = parts.next().unwrap_or("").trim();
        let ts = parts.next().unwrap_or("0").trim();
        let subject = parts.next().unwrap_or("").to_string();
        if sha.is_empty() {
            continue;
        }
        let author_ts = ts.parse::<i64>().unwrap_or(0);
        out.push(CommitMeta {
            sha: parse_sha("first_parent_commits", sha)?,
            author_ts,
            subject,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMeta {
    pub sha: CommitSha,
    pub author_ts: i64,
    pub subject: String,
}

/// Subject + extended message body for a single commit. Uses
/// `git show -s --format=%s%x00%b` so the patch text is never read or
/// returned — the caller's `message_body` is guaranteed not to contain
/// `diff --git` markers (that would happen if we parsed `show_commit`
/// output by hand). Subject is the first line; body is everything after
/// the blank line that separates the subject from the message body, or
/// empty when the commit has no extended body.
pub async fn commit_message(repo: &Path, sha: &CommitSha) -> Result<(String, String), GitIoError> {
    let stdout = run_ok(repo, &["show", "-s", "--format=%s%x00%b", sha.as_str()]).await?;
    let mut parts = stdout.splitn(2, '\0');
    let subject = parts
        .next()
        .unwrap_or("")
        .trim_end_matches('\n')
        .to_string();
    let body = parts
        .next()
        .unwrap_or("")
        .trim_end_matches('\n')
        .to_string();
    Ok((subject, body))
}

/// `git diff-tree -r --name-status -M --no-commit-id <sha>` parsed into a
/// `CommitChanges`. Renames are detected as `R<score>` entries with both
/// old and new paths.
///
/// For the root commit (no parent), uses `--root` form to enumerate its
/// added files.
///
/// For added/modified `.trinity/finished/<stem>/<file>` paths this also
/// shells out to `git show <sha>:<path>` to capture the body's first
/// non-empty line (what the finalize rule's APPROVE check reads).
pub async fn diff_tree_changes(repo: &Path, sha: &CommitSha) -> Result<CommitChanges, GitIoError> {
    // `-m --first-parent`: for merge commits, emit the diff against
    // the first parent (otherwise diff-tree suppresses merges,
    // hiding plan/finalize touches that landed via --no-ff). `--root`
    // is for the initial commit's "diff against the empty tree."
    let output = run(
        repo,
        &[
            "diff-tree",
            "-r",
            "-m",
            "--first-parent",
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
    let parsed = parse_diff_tree(&String::from_utf8_lossy(&output.stdout))?;
    let mut changes = parsed.changes;
    for upsert in parsed.finalize_upserts {
        let path = format!(
            ".trinity/finished/{}/{}",
            upsert.plan_key.as_str(),
            upsert.file_name
        );
        let body = run_ok(repo, &["show", &format!("{}:{}", sha.as_str(), path)]).await?;
        let first_line = body
            .lines()
            .map(str::trim_end)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string();
        changes.finalize_changes.push(FinalizeChange {
            plan_key: upsert.plan_key,
            file_name: upsert.file_name,
            kind: FinalizeChangeKind::Upsert { first_line },
        });
    }
    // Pre-fetch plan-file bodies for Add/Modify touches so the sans-IO
    // fold has the body needed to populate `state.plans` and to
    // capture body-at-freeze. The path-index pairing above stays
    // aligned with `changes.plan_touches` because `parse_diff_tree`
    // pushes plan_touches in stdout order and `plan_body_paths` is
    // built from the same loop in that same order.
    for (touch_idx, path) in parsed.plan_body_paths.into_iter() {
        let body = show_blob(repo, sha, &path).await?;
        changes.plan_touches[touch_idx].new_body = Some(body);
    }
    Ok(changes)
}

struct ParsedDiffTree {
    changes: CommitChanges,
    /// Finalize paths added or modified in this commit. The caller
    /// resolves each one's first-body-line via a `git show` call after
    /// parsing.
    finalize_upserts: Vec<FinalizeUpsertPath>,
    /// (plan_touch_index, path) pairs for plan-file touches whose
    /// `new_body` needs filling. The caller fetches each body via
    /// `git show <sha>:<path>` and writes it back into
    /// `changes.plan_touches[index].new_body`. Deletion touches are
    /// omitted (their `new_body` stays `None`).
    plan_body_paths: Vec<(usize, PathBuf)>,
}

struct FinalizeUpsertPath {
    plan_key: PlanKey,
    file_name: String,
}

fn parse_diff_tree(stdout: &str) -> Result<ParsedDiffTree, GitIoError> {
    let mut plan_touches: Vec<PlanTouch> = Vec::new();
    let mut has_non_plan_code_changes = false;
    let mut finalize_changes: Vec<FinalizeChange> = Vec::new();
    let mut finalize_upserts: Vec<FinalizeUpsertPath> = Vec::new();
    let mut plan_body_paths: Vec<(usize, PathBuf)> = Vec::new();

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

        let is_rename = status.starts_with('R') || status.starts_with('C');
        let new_path = if is_rename {
            second_path.unwrap_or(first_path)
        } else {
            first_path
        };
        let old_path = if is_rename { Some(first_path) } else { None };
        let status_char = status.chars().next().unwrap_or(' ');

        let new_rel = PathBuf::from(new_path);
        let new_is_plan = is_plan_path(&new_rel);
        let old_is_plan = old_path
            .map(|p| is_plan_path(&PathBuf::from(p)))
            .unwrap_or(false);

        let new_finalize = parse_finalize_subpath(&new_rel);
        let old_finalize = old_path.and_then(|p| parse_finalize_subpath(&PathBuf::from(p)));

        if new_is_plan || old_is_plan {
            // Resolve the plan key on each side. With nested-path
            // rejection (Phase 5 of event-log-and-finished), every
            // `.trinity/plans/X.md` path uniquely identifies stem X,
            // so old and new keys differ iff the rename crosses
            // stems.
            let new_key = if new_is_plan {
                PlanKey::from_path(&new_rel)
            } else {
                None
            };
            let old_key = old_path.and_then(|p| PlanKey::from_path(&PathBuf::from(p)));

            let is_deletion = status_char == 'D' && !is_rename;

            if let (true, Some(old_k), Some(new_k)) = (is_rename, &old_key, &new_key)
                && old_k != new_k
            {
                // Cross-stem rename `git mv .trinity/plans/foo.md
                // .trinity/plans/bar.md`. Model as two events: delete
                // `foo` + intro `bar`. The fold then removes `foo`
                // from state.plans (if not frozen) and creates `bar`.
                plan_touches.push(PlanTouch {
                    session: old_k.clone(),
                    kind: PlanTouchKind::Revision,
                    new_path: None,
                    new_body: None,
                });
                let touch_index = plan_touches.len();
                plan_touches.push(PlanTouch {
                    session: new_k.clone(),
                    kind: PlanTouchKind::Intro,
                    new_path: Some(new_rel.clone()),
                    new_body: None,
                });
                plan_body_paths.push((touch_index, new_rel.clone()));
            } else {
                let plan_key = match new_key.or(old_key) {
                    Some(id) => id,
                    None => continue,
                };
                let kind = match status_char {
                    'A' => PlanTouchKind::Intro,
                    _ => PlanTouchKind::Revision,
                };
                let new_path_for_touch = if is_deletion {
                    None
                } else {
                    Some(new_rel.clone())
                };
                let touch_index = plan_touches.len();
                plan_touches.push(PlanTouch {
                    session: plan_key,
                    kind,
                    new_path: new_path_for_touch,
                    new_body: None,
                });
                if !is_deletion {
                    plan_body_paths.push((touch_index, new_rel.clone()));
                }
            }
        } else if new_finalize.is_some() || old_finalize.is_some() {
            // Finalize-snapshot file. A rename whose <stem>/<file> differs
            // is modelled as Remove(old) + Upsert(new); a pure 'D' is a
            // Remove of the path in `new_rel` (git's name-status puts the
            // deleted path there for non-rename deletes).
            if status_char == 'D' && !is_rename {
                if let Some((plan_key, file_name)) = new_finalize.clone() {
                    finalize_changes.push(FinalizeChange {
                        plan_key,
                        file_name,
                        kind: FinalizeChangeKind::Remove,
                    });
                }
            } else if is_rename {
                if let Some((old_key, old_file)) = old_finalize.clone() {
                    let same = matches!(
                        &new_finalize,
                        Some((nk, nf)) if nk == &old_key && nf == &old_file
                    );
                    if !same {
                        finalize_changes.push(FinalizeChange {
                            plan_key: old_key,
                            file_name: old_file,
                            kind: FinalizeChangeKind::Remove,
                        });
                    }
                }
                if let Some((plan_key, file_name)) = new_finalize {
                    finalize_upserts.push(FinalizeUpsertPath {
                        plan_key,
                        file_name,
                    });
                }
            } else {
                // 'A' or 'M' (or any other non-D, non-rename status).
                if let Some((plan_key, file_name)) = new_finalize {
                    finalize_upserts.push(FinalizeUpsertPath {
                        plan_key,
                        file_name,
                    });
                }
            }
        } else if !new_rel.starts_with(".trinity") {
            has_non_plan_code_changes = true;
        }
    }

    Ok(ParsedDiffTree {
        changes: CommitChanges {
            plan_touches,
            has_non_plan_code_changes,
            finalize_changes,
        },
        finalize_upserts,
        plan_body_paths,
    })
}

fn parse_finalize_subpath(rel: &Path) -> Option<(PlanKey, String)> {
    let under = rel.strip_prefix(".trinity/finished").ok()?;
    let parsed = parse_finalize_path(under)?;
    let file_name = parsed
        .raw
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())?;
    Some((parsed.plan_key, file_name))
}

/// Gather a `DiskSnapshot` for `repo_root`. IO half of the rebuild
/// flow; `disk_snapshot::derive_state` consumes the result as a fold.
///
/// Three IO steps in order:
/// 1. `git rev-parse HEAD` (empty repo → empty snapshot).
/// 2. Per-commit walk along HEAD's first-parent chain. For each
///    commit: `git diff-tree` for the diff structure; `git show` per
///    plan-file Add/Modify (body) and per finalize-file upsert (first
///    line) so each `CommitEvent` carries everything the fold needs.
/// 3. Walk `<repo>/.trinity/feedback/` for working-tree feedback
///    (NOT in git).
pub async fn snapshot(repo_root: &Path) -> Result<DiskSnapshot, GitIoError> {
    let head = rev_parse_head(repo_root).await?;
    let Some(head) = head else {
        return Ok(DiskSnapshot::default());
    };

    let metas = first_parent_commits(repo_root).await?;
    let mut history: Vec<CommitEvent> = Vec::with_capacity(metas.len());
    for meta in metas {
        let changes = diff_tree_changes(repo_root, &meta.sha).await?;
        history.push(CommitEvent {
            commit: meta.sha,
            author_ts: meta.author_ts,
            subject: meta.subject,
            changes,
        });
    }

    let feedback_files = collect_feedback_files(repo_root)?;

    Ok(DiskSnapshot {
        head: Some(head),
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
    // `.trinity/plans/<name>.md` (no nested subdirs).
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
    let third = match comps.next() {
        Some(s) => s,
        None => return false,
    };
    comps.next().is_none() && third.ends_with(".md")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_plan_path_active() {
        assert!(is_plan_path(&PathBuf::from(".trinity/plans/foo.md")));
    }

    #[test]
    fn is_plan_path_rejects_done_subdir() {
        assert!(!is_plan_path(&PathBuf::from(".trinity/plans/done/foo.md")));
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
        let parsed = parse_diff_tree(stdout).unwrap();
        let changes = &parsed.changes;
        assert_eq!(changes.plan_touches.len(), 1);
        assert_eq!(changes.plan_touches[0].session.as_str(), "foo");
        assert!(matches!(changes.plan_touches[0].kind, PlanTouchKind::Intro));
        assert_eq!(
            changes.plan_touches[0].new_path.as_deref(),
            Some(Path::new(".trinity/plans/foo.md"))
        );
        assert!(!changes.has_non_plan_code_changes);
    }

    #[test]
    fn parse_diff_tree_plan_revision_with_code() {
        let stdout = "M\t.trinity/plans/foo.md\nM\tsrc/lib.rs\n";
        let parsed = parse_diff_tree(stdout).unwrap();
        let changes = &parsed.changes;
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
        let parsed = parse_diff_tree(stdout).unwrap();
        let changes = &parsed.changes;
        assert!(changes.plan_touches.is_empty());
        assert!(changes.has_non_plan_code_changes);
    }

    #[test]
    fn parse_diff_tree_multi_plan_touch() {
        let stdout = "M\t.trinity/plans/foo.md\nA\t.trinity/plans/bar.md\nM\tsrc/lib.rs\n";
        let parsed = parse_diff_tree(stdout).unwrap();
        let changes = &parsed.changes;
        assert_eq!(changes.plan_touches.len(), 2);
        assert!(changes.has_non_plan_code_changes);
    }

    #[test]
    fn parse_diff_tree_ignores_other_trinity_paths() {
        let stdout = "A\t.trinity/feedback/foo/plan/alice.md\n";
        let parsed = parse_diff_tree(stdout).unwrap();
        let changes = &parsed.changes;
        assert!(changes.plan_touches.is_empty());
        assert!(!changes.has_non_plan_code_changes);
        assert!(changes.finalize_changes.is_empty());
    }

    #[test]
    fn parse_diff_tree_finalize_added_queues_upsert() {
        let stdout = "A\t.trinity/finished/foo/alice.md\n";
        let parsed = parse_diff_tree(stdout).unwrap();
        assert!(parsed.changes.plan_touches.is_empty());
        assert!(!parsed.changes.has_non_plan_code_changes);
        assert!(parsed.changes.finalize_changes.is_empty());
        assert_eq!(parsed.finalize_upserts.len(), 1);
        assert_eq!(parsed.finalize_upserts[0].plan_key.as_str(), "foo");
        assert_eq!(parsed.finalize_upserts[0].file_name, "alice.md");
    }

    #[test]
    fn parse_diff_tree_finalize_deleted_emits_remove() {
        let stdout = "D\t.trinity/finished/foo/alice.md\n";
        let parsed = parse_diff_tree(stdout).unwrap();
        assert_eq!(parsed.changes.finalize_changes.len(), 1);
        let fc = &parsed.changes.finalize_changes[0];
        assert_eq!(fc.plan_key.as_str(), "foo");
        assert_eq!(fc.file_name, "alice.md");
        assert!(matches!(fc.kind, FinalizeChangeKind::Remove));
        assert!(parsed.finalize_upserts.is_empty());
    }
}
