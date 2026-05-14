//! Trinity runtime: owns the in-memory `Trinity` state and handles
//! filesystem signals coming in from the watcher layer.
//!
//! This module is the bridge between the pure core (rebuild + reducer)
//! and the rest of the daemon. The notify wiring (separate, not yet
//! written) feeds `FilesystemSignal` values into `handle_signal`;
//! request handlers (MCP, HTTP) read state via `read_repo`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::disk_format::FeedbackPhase;
use crate::fs_watcher::FilesystemSignal;
use crate::lifecycle::content_hash;
use crate::rebuild::{RebuildError, rebuild_repo};
use crate::repo_state::{Feedback, HeldFeedback, LiveEvent, RepoState, Session, Trinity};

#[derive(Default)]
pub struct Runtime {
    state: Arc<Mutex<Trinity>>,
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("rebuild: {0}")]
    Rebuild(#[from] RebuildError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("unknown repo: {0}")]
    UnknownRepo(PathBuf),
}

impl Runtime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> Arc<Mutex<Trinity>> {
        Arc::clone(&self.state)
    }

    /// Register a repo: run an initial rebuild and insert into state.
    /// Idempotent — re-calling against an existing repo re-runs the rebuild.
    pub async fn add_repo(&self, repo_root: PathBuf) -> Result<(), RuntimeError> {
        let fresh = rebuild_repo(&repo_root).await?;
        let mut trinity = self.state.lock().await;
        trinity.repos.insert(repo_root, fresh);
        Ok(())
    }

    /// Read-only snapshot of a repo's state for request handlers. Holds the
    /// mutex for the duration of the closure.
    pub async fn read_repo<R>(
        &self,
        repo_root: &Path,
        f: impl FnOnce(&RepoState) -> R,
    ) -> Result<R, RuntimeError> {
        let trinity = self.state.lock().await;
        let state = trinity
            .repos
            .get(repo_root)
            .ok_or_else(|| RuntimeError::UnknownRepo(repo_root.to_path_buf()))?;
        Ok(f(state))
    }

    /// Recent live events for SSE replay on reconnect. Bounded by the
    /// ring buffer's capacity (oldest dropped as new events arrive).
    pub async fn live_events_snapshot(&self) -> VecDeque<LiveEvent> {
        let trinity = self.state.lock().await;
        trinity.live_events.clone()
    }

    /// Handle one `FilesystemSignal` from the watcher layer.
    ///
    /// - `HeadChanged` → full rebuild of the repo, broadcast a single
    ///   `repo_rebuilt` event.
    /// - `PlanFileChanged` → narrow: status is computed at read time;
    ///   broadcast a `plan_worktree_changed` event for live UI updates.
    /// - `FeedbackWritten` → reload feedback for the affected session;
    ///   broadcast `feedback_changed`.
    /// - `FeedbackRemoved` → drop the corresponding feedback entry;
    ///   broadcast `feedback_removed`.
    pub async fn handle_signal(
        &self,
        repo_root: &Path,
        signal: FilesystemSignal,
        now: i64,
    ) -> Result<(), RuntimeError> {
        match signal {
            FilesystemSignal::HeadChanged => {
                let fresh = rebuild_repo(repo_root).await?;
                let mut trinity = self.state.lock().await;
                trinity.repos.insert(repo_root.to_path_buf(), fresh);
                push_event(
                    &mut trinity.live_events,
                    LiveEvent {
                        ts: now,
                        repo: repo_root.to_path_buf(),
                        session_id: None,
                        kind: "repo_rebuilt",
                        payload: serde_json::Value::Null,
                    },
                );
            }
            FilesystemSignal::PlanFileChanged { session_id, path } => {
                let mut trinity = self.state.lock().await;
                let Some(state) = trinity.repos.get(repo_root) else {
                    return Err(RuntimeError::UnknownRepo(repo_root.to_path_buf()));
                };
                if !state.sessions.contains_key(&session_id) {
                    // Untracked draft — drop silently per the plan.
                    return Ok(());
                }
                push_event(
                    &mut trinity.live_events,
                    LiveEvent {
                        ts: now,
                        repo: repo_root.to_path_buf(),
                        session_id: Some(session_id),
                        kind: "plan_worktree_changed",
                        payload: serde_json::json!({"path": path.to_string_lossy()}),
                    },
                );
            }
            FilesystemSignal::FeedbackWritten { parsed } => {
                let abs_path = repo_root
                    .join(".trinity/feedback")
                    .join(&parsed.raw);
                let body = match std::fs::read_to_string(&abs_path) {
                    Ok(b) => b,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(e) => return Err(e.into()),
                };
                let mut trinity = self.state.lock().await;
                let Some(state) = trinity.repos.get_mut(repo_root) else {
                    return Err(RuntimeError::UnknownRepo(repo_root.to_path_buf()));
                };
                let session_id = parsed.session_id.clone();
                let Some(session) = state.sessions.get_mut(&session_id) else {
                    return Ok(());
                };
                upsert_feedback(session, abs_path, parsed, body);
                push_event(
                    &mut trinity.live_events,
                    LiveEvent {
                        ts: now,
                        repo: repo_root.to_path_buf(),
                        session_id: Some(session_id),
                        kind: "feedback_changed",
                        payload: serde_json::Value::Null,
                    },
                );
            }
            FilesystemSignal::FeedbackRemoved { parsed } => {
                let mut trinity = self.state.lock().await;
                let Some(state) = trinity.repos.get_mut(repo_root) else {
                    return Err(RuntimeError::UnknownRepo(repo_root.to_path_buf()));
                };
                let session_id = parsed.session_id.clone();
                let Some(session) = state.sessions.get_mut(&session_id) else {
                    return Ok(());
                };
                remove_feedback(session, &parsed);
                push_event(
                    &mut trinity.live_events,
                    LiveEvent {
                        ts: now,
                        repo: repo_root.to_path_buf(),
                        session_id: Some(session_id),
                        kind: "feedback_removed",
                        payload: serde_json::Value::Null,
                    },
                );
            }
        }
        Ok(())
    }
}

fn push_event(ring: &mut VecDeque<LiveEvent>, event: LiveEvent) {
    const CAPACITY: usize = 200;
    if ring.len() >= CAPACITY {
        ring.pop_front();
    }
    ring.push_back(event);
}

fn upsert_feedback(
    session: &mut Session,
    abs_path: PathBuf,
    parsed: crate::disk_format::FeedbackPath,
    body: String,
) {
    let verdict = crate::disk_format::parse_verdict(&body);
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
            let reason = match parsed.phase {
                FeedbackPhase::Plan => "plan_dirty",
                FeedbackPhase::Impl => "impl_flat_drop",
            };
            session.held_plan_feedback.push(HeldFeedback {
                path: abs_path,
                author: parsed.author,
                body,
                reason,
            });
            let _ = verdict;
        }
    }
}

fn remove_feedback(session: &mut Session, parsed: &crate::disk_format::FeedbackPath) {
    match &parsed.target_sha {
        Some(target_sha) => {
            let map = match parsed.phase {
                FeedbackPhase::Plan => &mut session.plan_feedback,
                FeedbackPhase::Impl => &mut session.impl_feedback,
            };
            map.remove(&(target_sha.clone(), parsed.author.clone()));
        }
        None => {
            session
                .held_plan_feedback
                .retain(|h| h.author != parsed.author);
        }
    }
}

// Silence dead-code warnings on imports only used in specific branches.
#[allow(dead_code)]
fn _hash_ref(s: &str) -> crate::lifecycle::ContentHash {
    content_hash(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{AgentLabel, CommitSha, SessionId};
    use crate::mcp_response::{get_context_response, list_sessions_response};
    use std::path::Path;
    use std::process::Command;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
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
            .unwrap();
        assert!(status.success());
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
    async fn add_repo_loads_initial_state() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let count = rt
            .read_repo(dir.path(), |s| s.sessions.len())
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn head_changed_triggers_full_rebuild() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();
        let count0 = rt
            .read_repo(dir.path(), |s| s.attribution.len())
            .await
            .unwrap();

        // New commit — plan revision
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2\n");
        commit(dir.path(), "revise foo");

        rt.handle_signal(dir.path(), FilesystemSignal::HeadChanged, 1)
            .await
            .unwrap();

        let count1 = rt
            .read_repo(dir.path(), |s| s.attribution.len())
            .await
            .unwrap();
        assert!(count1 > count0, "attribution should grow after rebuild");

        let events = rt.live_events_snapshot().await;
        assert!(events.iter().any(|e| e.kind == "repo_rebuilt"));
    }

    #[tokio::test]
    async fn plan_file_changed_broadcasts_for_known_session() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        rt.handle_signal(
            dir.path(),
            FilesystemSignal::PlanFileChanged {
                session_id: SessionId::from("foo".to_string()),
                path: PathBuf::from(".trinity/plans/foo.md"),
            },
            42,
        )
        .await
        .unwrap();

        let events = rt.live_events_snapshot().await;
        assert!(events.iter().any(|e| e.kind == "plan_worktree_changed"
            && e.session_id.as_ref().map(|s| s.as_str()) == Some("foo")));
    }

    #[tokio::test]
    async fn plan_file_changed_for_unknown_session_is_silently_dropped() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        // Untracked draft — session doesn't exist in HEAD.
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::PlanFileChanged {
                session_id: SessionId::from("unknown".to_string()),
                path: PathBuf::from(".trinity/plans/unknown.md"),
            },
            42,
        )
        .await
        .unwrap();

        let events = rt.live_events_snapshot().await;
        assert!(events.iter().all(|e| e.kind != "plan_worktree_changed"));
    }

    #[tokio::test]
    async fn feedback_written_loads_into_session_map() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let intro: CommitSha = rt
            .read_repo(dir.path(), |s| {
                s.sessions[&SessionId::from("foo".to_string())]
                    .plan_intro
                    .clone()
            })
            .await
            .unwrap();

        // Write feedback file on disk first.
        let feedback_rel = format!(".trinity/feedback/foo/plan/{}/alice.md", intro.as_str());
        write_file(dir.path(), &feedback_rel, "APPROVE\n\nlgtm\n");

        // Build the parsed FeedbackPath manually for the signal.
        let parsed_rel = PathBuf::from(format!("foo/plan/{}/alice.md", intro.as_str()));
        let parsed = crate::disk_format::parse_feedback_path(&parsed_rel).unwrap();

        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten { parsed },
            7,
        )
        .await
        .unwrap();

        // The gate should now resolve to ready_to_implement.
        let v = rt
            .read_repo(dir.path(), |s| {
                get_context_response(
                    dir.path(),
                    s,
                    &SessionId::from("foo".to_string()),
                    &AgentLabel::from("master".to_string()),
                )
            })
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(v["waiting_on"]["reason"], "ready_to_implement");

        let events = rt.live_events_snapshot().await;
        assert!(events.iter().any(|e| e.kind == "feedback_changed"));
    }

    #[tokio::test]
    async fn feedback_removed_clears_entry() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let intro: CommitSha = rt
            .read_repo(dir.path(), |s| {
                s.sessions[&SessionId::from("foo".to_string())]
                    .plan_intro
                    .clone()
            })
            .await
            .unwrap();
        let feedback_rel = format!(".trinity/feedback/foo/plan/{}/alice.md", intro.as_str());
        write_file(dir.path(), &feedback_rel, "APPROVE\n");
        let parsed_rel = PathBuf::from(format!("foo/plan/{}/alice.md", intro.as_str()));
        let parsed = crate::disk_format::parse_feedback_path(&parsed_rel).unwrap();
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten {
                parsed: parsed.clone(),
            },
            1,
        )
        .await
        .unwrap();

        // Remove the file and signal.
        std::fs::remove_file(dir.path().join(&feedback_rel)).unwrap();
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackRemoved { parsed },
            2,
        )
        .await
        .unwrap();

        let v = rt
            .read_repo(dir.path(), |s| {
                get_context_response(
                    dir.path(),
                    s,
                    &SessionId::from("foo".to_string()),
                    &AgentLabel::from("master".to_string()),
                )
            })
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        // Gate falls back to no participants → reviewers / plan_needs_initial_review.
        assert_eq!(v["waiting_on"]["reason"], "plan_needs_initial_review");
    }

    #[tokio::test]
    async fn list_sessions_via_runtime() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        write_file(dir.path(), ".trinity/plans/bar.md", "# bar\n");
        commit(dir.path(), "add bar");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let v = rt
            .read_repo(dir.path(), |s| list_sessions_response(dir.path(), s))
            .await
            .unwrap()
            .unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[tokio::test]
    async fn ring_buffer_bounded_at_capacity() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        for i in 0..210 {
            rt.handle_signal(
                dir.path(),
                FilesystemSignal::PlanFileChanged {
                    session_id: SessionId::from("foo".to_string()),
                    path: PathBuf::from(".trinity/plans/foo.md"),
                },
                i,
            )
            .await
            .unwrap();
        }

        let events = rt.live_events_snapshot().await;
        assert!(events.len() <= 200);
    }
}
