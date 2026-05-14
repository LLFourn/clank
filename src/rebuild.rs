//! Cold-start and HEAD-change rebuild for the filesystem-truth model.
//! One entry point — `rebuild_repo` — does both jobs. Called once per
//! known repo at startup, and again on every observed HEAD change.
//!
//! Composes `git_io` reads with the pure `attribution::classify` walk
//! and emits a fresh `RepoState`. Feedback file loading and the
//! post-rebuild normalization sweep land in this module too so the
//! whole "read git + working tree, build state" sequence is one
//! function.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::attribution::{classify, effective_session};
use crate::disk_format::{
    FeedbackPath, FeedbackPhase, parse_feedback_path, parse_verdict, session_id_from_plan_path,
};
use crate::git_io::{self, GitIoError};
use crate::lifecycle::{SessionId, content_hash};
use crate::repo_state::{Feedback, HeldFeedback, RepoState, Session, Verdict};

#[derive(Debug, thiserror::Error)]
pub enum RebuildError {
    #[error("git io: {0}")]
    Git(#[from] GitIoError),
    #[error("filesystem: {0}")]
    Fs(#[from] std::io::Error),
}

/// Cold-start-style rebuild for a single repo. Reads HEAD's tree to
/// discover sessions, walks attribution from the oldest plan_intro
/// forward, loads feedback files from the working tree, and produces
/// a fresh `RepoState`.
///
/// Empty repos (no commits) produce an empty `RepoState` (no sessions).
pub async fn rebuild_repo(repo_root: &Path) -> Result<RepoState, RebuildError> {
    let mut state = RepoState::empty(repo_root.to_path_buf());
    let head = git_io::rev_parse_head(repo_root).await?;
    state.head = head.clone();

    let Some(head) = head else {
        return Ok(state);
    };

    // 1. Discover sessions via `git ls-tree HEAD -- .trinity/plans/`.
    let plan_entries = git_io::ls_tree_plans(repo_root, &head).await?;
    for entry in &plan_entries {
        let Some(session_id) = session_id_from_plan_path(&entry.path) else {
            continue;
        };
        let body = git_io::show_blob(repo_root, &head, &entry.path).await?;
        let body_hash = content_hash(&body);
        let plan_intro = match git_io::first_added_commit(repo_root, &entry.path).await? {
            Some(sha) => sha,
            None => continue, // shouldn't happen for an ls-tree entry
        };
        let plan_intro_parent = git_io::parent_of(repo_root, &plan_intro).await?;
        state.sessions.insert(
            session_id.clone(),
            Session {
                id: session_id,
                plan_path: entry.path.clone(),
                body,
                body_hash,
                plan_intro,
                plan_intro_parent,
                plan_feedback: BTreeMap::new(),
                impl_feedback: BTreeMap::new(),
                held_plan_feedback: Vec::new(),
            },
        );
    }

    // 2. Walk attribution along the first-parent chain from the root to
    //    HEAD, oldest first. We don't bound this by `<intro>..HEAD` because
    //    identifying the topologically earliest plan_intro across multiple
    //    sessions requires a separate query; walking from the root is
    //    cheap for Trinity-scoped repos and avoids subtle correctness
    //    bugs around lower-bound selection.
    if !state.sessions.is_empty() {
        let commits = git_io::first_parent_commits(repo_root).await?;
        let mut current_effective: Option<SessionId> = None;
        for sha in &commits {
            let changes = git_io::diff_tree_changes(repo_root, sha).await?;
            let result = classify(&changes, current_effective.as_ref());
            current_effective = effective_session(&changes, current_effective.as_ref());
            state.attribution.insert(sha.clone(), result);
        }
    }

    // 3. Load feedback files (working tree).
    load_feedback(repo_root, &mut state)?;

    Ok(state)
}

fn load_feedback(repo_root: &Path, state: &mut RepoState) -> Result<(), RebuildError> {
    let feedback_root = repo_root.join(".trinity").join("feedback");
    if !feedback_root.exists() {
        return Ok(());
    }
    let mut found = Vec::new();
    walk_files(&feedback_root, 4, &mut found)?;
    for abs in found {
        let rel = match abs.strip_prefix(&feedback_root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        let Some(parsed) = parse_feedback_path(&rel) else {
            continue;
        };
        let body = std::fs::read_to_string(&abs)?;
        ingest_one_feedback(state, abs.clone(), parsed, body);
    }
    Ok(())
}

/// Plain recursive directory walk bounded by `max_depth` from the root.
/// Used to avoid the `walkdir` dep; the feedback tree is at most four
/// segments deep (`<session>/<phase>/[<sha>]/<author>.md`).
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

fn ingest_one_feedback(
    state: &mut RepoState,
    abs_path: PathBuf,
    parsed: FeedbackPath,
    body: String,
) {
    let Some(session) = state.sessions.get_mut(&parsed.session_id) else {
        // Feedback for an unknown session id (untracked draft, etc.). Skip.
        return;
    };
    let verdict = parse_verdict(&body);

    match parsed.target_sha {
        Some(target_sha) => {
            let map = match parsed.phase {
                FeedbackPhase::Plan => &mut session.plan_feedback,
                FeedbackPhase::Impl => &mut session.impl_feedback,
            };
            map.insert(
                (target_sha, parsed.author),
                Feedback {
                    path: abs_path,
                    body,
                    verdict,
                },
            );
        }
        None => {
            // Flat-drop feedback. For impl phase, gets auto-organized by
            // the runner immediately. For plan phase, this is what the
            // runner holds when the plan is body_dirty. We can't tell
            // from in-memory state alone whether the file should be held
            // or moved — that's the runner's call after computing
            // plan_worktree_status. For now: record as held under the
            // "plan_dirty" reason for plan files; let the caller (the
            // runner / route handler) decide whether to release it.
            if matches!(parsed.phase, FeedbackPhase::Plan) {
                session.held_plan_feedback.push(HeldFeedback {
                    path: abs_path,
                    author: parsed.author,
                    body,
                    reason: "plan_dirty",
                });
            } else {
                // Impl flat drop. The runner moves it to the canonical
                // path once it knows the current impl target SHA. For
                // load_feedback we just hold the data so the runner can
                // act on it — store as held with a different reason key.
                session.held_plan_feedback.push(HeldFeedback {
                    path: abs_path,
                    author: parsed.author,
                    body,
                    reason: "impl_flat_drop",
                });
            }
        }
    }

    let _ = verdict; // silence warning if unused in some branch
}

// Drop unused `Verdict` import only used inside fn above; keep visible.
#[allow(dead_code)]
fn _verdict_marker(v: Verdict) -> &'static str {
    v.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_state::AttributionResult;
    use std::process::Command;

    /// Spin up a tempdir with a real git repo. Returns the repo root.
    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        run_git(path, &["init", "--quiet", "--initial-branch=main"]);
        run_git(path, &["config", "user.email", "test@test"]);
        run_git(path, &["config", "user.name", "test"]);
        run_git(path, &["config", "commit.gpgsign", "false"]);
        dir
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .expect("git");
        assert!(status.success(), "git {:?} failed", args);
    }

    fn write_file(repo: &Path, rel: &str, body: &str) {
        let abs = repo.join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(abs, body).unwrap();
    }

    fn commit(repo: &Path, msg: &str) {
        run_git(repo, &["add", "-A"]);
        run_git(repo, &["commit", "--quiet", "-m", msg]);
    }

    #[tokio::test]
    async fn empty_repo_yields_empty_state() {
        let dir = init_repo();
        let state = rebuild_repo(dir.path()).await.unwrap();
        assert!(state.head.is_none());
        assert!(state.sessions.is_empty());
        assert!(state.attribution.is_empty());
    }

    #[tokio::test]
    async fn single_plan_commit_creates_session() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "Add foo plan");

        let state = rebuild_repo(dir.path()).await.unwrap();
        assert_eq!(state.sessions.len(), 1);
        let session = &state.sessions[&SessionId::from("foo".to_string())];
        assert_eq!(session.id.as_str(), "foo");
        assert_eq!(session.body, "# foo\n");

        // Attribution map has the plan-intro commit attributed to foo.
        let intro = &session.plan_intro;
        let attr = state.attribution.get(intro).expect("intro in attribution");
        assert!(matches!(
            attr,
            AttributionResult::Attributed { session, plan_touch: Some(_), has_code_changes: false } if session.as_str() == "foo"
        ));
    }

    #[tokio::test]
    async fn impl_commit_attributes_via_walkback() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "Add foo plan");
        write_file(dir.path(), "src/lib.rs", "fn main() {}\n");
        commit(dir.path(), "Implement foo");

        let state = rebuild_repo(dir.path()).await.unwrap();
        assert_eq!(state.attribution.len(), 2);

        // Find the impl commit (has_code_changes = true, no plan_touch)
        let impl_attrs: Vec<_> = state
            .attribution
            .values()
            .filter(|a| matches!(a, AttributionResult::Attributed { has_code_changes: true, plan_touch: None, .. }))
            .collect();
        assert_eq!(impl_attrs.len(), 1);
        if let AttributionResult::Attributed { session, .. } = impl_attrs[0] {
            assert_eq!(session.as_str(), "foo");
        }
    }

    #[tokio::test]
    async fn mixed_commit_is_both_plan_and_impl() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# v1\n");
        commit(dir.path(), "Add foo plan");
        write_file(dir.path(), ".trinity/plans/foo.md", "# v2\n");
        write_file(dir.path(), "src/lib.rs", "fn main() {}\n");
        commit(dir.path(), "Revise plan + impl");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let mixed: Vec<_> = state
            .attribution
            .values()
            .filter(|a| matches!(a, AttributionResult::Attributed { plan_touch: Some(_), has_code_changes: true, .. }))
            .collect();
        assert_eq!(mixed.len(), 1, "expected exactly one mixed commit");
    }

    #[tokio::test]
    async fn switching_sessions_via_plan_touch_commit() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "Add foo plan");
        write_file(dir.path(), "src/foo.rs", "// foo impl\n");
        commit(dir.path(), "foo impl 1");
        // Switch to bar by committing bar's plan
        write_file(dir.path(), ".trinity/plans/bar.md", "# bar\n");
        commit(dir.path(), "Add bar plan");
        write_file(dir.path(), "src/bar.rs", "// bar impl\n");
        commit(dir.path(), "bar impl 1");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let mut sessions: Vec<&str> = state
            .attribution
            .values()
            .filter_map(|a| match a {
                AttributionResult::Attributed {
                    session,
                    has_code_changes: true,
                    plan_touch: None,
                    ..
                } => Some(session.as_str()),
                _ => None,
            })
            .collect();
        sessions.sort();
        // Both impl commits should attribute, one to each session. (Order
        // here is by SHA, not commit order; semantic correctness is that
        // both are present and distinct.)
        assert_eq!(sessions, vec!["bar", "foo"]);
    }

    #[tokio::test]
    async fn done_move_is_classified() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "Add foo plan");
        // Do the move (no `git mv` — plain rename)
        let from = dir.path().join(".trinity/plans/foo.md");
        let to_dir = dir.path().join(".trinity/plans/done");
        std::fs::create_dir_all(&to_dir).unwrap();
        std::fs::rename(from, to_dir.join("foo.md")).unwrap();
        commit(dir.path(), "Move foo to done");

        let state = rebuild_repo(dir.path()).await.unwrap();
        // Session should be discovered at the done path.
        let session = &state.sessions[&SessionId::from("foo".to_string())];
        assert!(
            session.plan_path.to_string_lossy().contains("done/"),
            "plan_path should be under done/, got {}",
            session.plan_path.display()
        );
    }

    #[tokio::test]
    async fn feedback_file_at_target_sha_loads() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "Add foo plan");
        // Look up the plan_intro SHA — that's what plan reviews target.
        let state0 = rebuild_repo(dir.path()).await.unwrap();
        let intro = state0.sessions[&SessionId::from("foo".to_string())]
            .plan_intro
            .clone();
        let intro_str = intro.as_str();
        // Use first 7 chars as the target dir.
        let short = &intro_str[..7];
        let feedback_rel = format!(".trinity/feedback/foo/plan/{}/alice.md", short);
        write_file(dir.path(), &feedback_rel, "APPROVE\n\nLooks good.\n");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let session = &state.sessions[&SessionId::from("foo".to_string())];
        let entries: Vec<_> = session.plan_feedback.values().collect();
        assert_eq!(entries.len(), 1, "expected one plan feedback entry");
        assert_eq!(entries[0].verdict, Verdict::Approve);
    }
}
