//! Trinity runtime: owns the in-memory `Trinity` state and handles
//! filesystem signals coming in from the watcher layer.
//!
//! This module is the bridge between the pure core (rebuild + reducer)
//! and the rest of the daemon. The notify wiring (separate, not yet
//! written) feeds `FilesystemSignal` values into `handle_signal`;
//! request handlers (MCP, HTTP) read state via owned snapshots.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, broadcast};

use crate::disk_format::FeedbackPhase;
use crate::fs_watcher::FilesystemSignal;
use crate::lifecycle::{CommitSha, SessionId, content_hash};
use crate::rebuild::{RebuildError, rebuild_repo};
use crate::repo_state::{AttributionResult, Feedback, HeldFeedback, LiveEvent, Session, Trinity};
use crate::runtime_snapshot::{RepoSnapshot, SessionSnapshotBundle};

pub struct Runtime {
    state: Arc<Mutex<Trinity>>,
    /// Paths Trinity recently wrote (renames during feedback
    /// auto-organization). Watcher events on these paths are suppressed
    /// for ~1 second so the runtime doesn't reprocess its own edits.
    self_writes: Arc<Mutex<HashMap<PathBuf, Instant>>>,
    /// Broadcast channel for live events. SSE handlers subscribe here.
    events_tx: broadcast::Sender<LiveEvent>,
}

impl Default for Runtime {
    fn default() -> Self {
        let (events_tx, _) = broadcast::channel(256);
        Self {
            state: Arc::default(),
            self_writes: Arc::default(),
            events_tx,
        }
    }
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
    /// Canonicalizes the path so callers from different surfaces (test
    /// tempdir vs `git rev-parse --show-toplevel` resolution) hit the same
    /// HashMap key. Runs the held-feedback normalization sweep after the
    /// initial load so flat-drop feedback that pre-dated this boot gets
    /// auto-organized into `<phase>/<target-sha>/<author>.md`.
    pub async fn add_repo(&self, repo_root: PathBuf) -> Result<(), RuntimeError> {
        let canonical = dunce::canonicalize(&repo_root).unwrap_or(repo_root);
        let fresh = rebuild_repo(&canonical).await?;
        {
            let mut trinity = self.state.lock().await;
            trinity.repos.insert(canonical.clone(), fresh);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.normalize_held_feedback(&canonical, now).await?;
        Ok(())
    }

    /// Register a repo only if it isn't already known. Silently swallows
    /// rebuild errors (logged), so request handlers can call this on
    /// every request without needing to special-case "already loaded."
    pub async fn add_repo_if_unknown(&self, repo_root: PathBuf) {
        let canonical = dunce::canonicalize(&repo_root).unwrap_or(repo_root);
        {
            let trinity = self.state.lock().await;
            if trinity.repos.contains_key(&canonical) {
                return;
            }
        }
        if let Err(err) = self.add_repo(canonical.clone()).await {
            tracing::warn!(repo = %canonical.display(), error = ?err, "add_repo_if_unknown failed");
        }
    }

    /// Clone a repo snapshot while holding the runtime lock. Callers do
    /// disk and git I/O after this returns.
    pub async fn snapshot_repo(&self, repo_root: &Path) -> Result<RepoSnapshot, RuntimeError> {
        let canonical = dunce::canonicalize(repo_root)
            .unwrap_or_else(|_| repo_root.to_path_buf());
        let trinity = self.state.lock().await;
        let state = trinity
            .repos
            .get(&canonical)
            .ok_or_else(|| RuntimeError::UnknownRepo(canonical.clone()))?;
        Ok(RepoSnapshot::from_state(state))
    }

    /// Clone one session plus the repo-level indexes its projections need.
    /// `Ok(None)` means the repo is known but the session is not committed.
    pub async fn snapshot_session(
        &self,
        repo_root: &Path,
        session_id: &SessionId,
    ) -> Result<Option<SessionSnapshotBundle>, RuntimeError> {
        let canonical = dunce::canonicalize(repo_root)
            .unwrap_or_else(|_| repo_root.to_path_buf());
        let trinity = self.state.lock().await;
        let state = trinity
            .repos
            .get(&canonical)
            .ok_or_else(|| RuntimeError::UnknownRepo(canonical.clone()))?;
        Ok(SessionSnapshotBundle::from_state_for(state, session_id))
    }

    /// Read-only in-memory access for tests. Do not call disk-aware
    /// helpers from the closure.
    #[cfg(test)]
    pub(crate) async fn read_repo<R>(
        &self,
        repo_root: &Path,
        f: impl FnOnce(&crate::repo_state::RepoState) -> R,
    ) -> Result<R, RuntimeError> {
        let canonical = dunce::canonicalize(repo_root)
            .unwrap_or_else(|_| repo_root.to_path_buf());
        let trinity = self.state.lock().await;
        let state = trinity
            .repos
            .get(&canonical)
            .ok_or_else(|| RuntimeError::UnknownRepo(canonical.clone()))?;
        Ok(f(state))
    }

    /// Recent live events for SSE replay on reconnect. Bounded by the
    /// ring buffer's capacity (oldest dropped as new events arrive).
    pub async fn live_events_snapshot(&self) -> VecDeque<LiveEvent> {
        let trinity = self.state.lock().await;
        trinity.live_events.clone()
    }

    /// Subscribe to the live event broadcast channel. SSE handlers use
    /// this to receive new events as they're appended (no polling).
    pub fn subscribe_events(&self) -> broadcast::Receiver<LiveEvent> {
        self.events_tx.subscribe()
    }

    /// Mark paths Trinity is about to rename so the watcher loop can
    /// suppress the resulting events. Entries expire after 1 second.
    async fn mark_self_writes(&self, paths: &[PathBuf]) {
        let mut ring = self.self_writes.lock().await;
        let now = Instant::now();
        for p in paths {
            ring.insert(p.clone(), now);
        }
    }

    /// True if `path` was recently written by Trinity. Removes the entry
    /// on hit (so the same path can be re-touched later by the user
    /// and we won't keep suppressing).
    async fn should_skip_self_write(&self, path: &Path) -> bool {
        let mut ring = self.self_writes.lock().await;
        // Garbage-collect expired entries opportunistically.
        let cutoff = Instant::now() - std::time::Duration::from_secs(5);
        ring.retain(|_, ts| *ts > cutoff);
        if let Some(ts) = ring.remove(path) {
            ts.elapsed() < std::time::Duration::from_secs(1)
        } else {
            false
        }
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
        let repo_root_canonical = dunce::canonicalize(repo_root)
            .unwrap_or_else(|_| repo_root.to_path_buf());
        let repo_root = repo_root_canonical.as_path();
        match signal {
            FilesystemSignal::HeadChanged => {
                let fresh = rebuild_repo(repo_root).await?;
                let fresh_digest = fresh.digest();
                let changed = {
                    let mut trinity = self.state.lock().await;
                    let prior_digest = trinity.repos.get(repo_root).map(|r| r.digest());
                    let changed = prior_digest.as_ref() != Some(&fresh_digest);
                    trinity.repos.insert(repo_root.to_path_buf(), fresh);
                    if changed {
                        self.push_event(
                            &mut trinity,
                            LiveEvent {
                                ts: now,
                                repo: repo_root.to_path_buf(),
                                session_id: None,
                                kind: "repo_rebuilt",
                                payload: serde_json::Value::Null,
                            },
                        );
                    }
                    changed
                };
                // Held-feedback normalization sweep runs even when nothing
                // visibly changed in the rebuild — a watcher-induced rebuild
                // might still need to release a flat-drop file. The sweep
                // itself emits events only on actual file moves.
                self.normalize_held_feedback(repo_root, now).await?;
                let _ = changed; // tracing hook for future "rebuild was a no-op" log
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
                self.push_event(
                    &mut trinity,
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
                if self.should_skip_self_write(&abs_path).await {
                    return Ok(());
                }
                let body = match std::fs::read_to_string(&abs_path) {
                    Ok(b) => b,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(e) => return Err(e.into()),
                };
                let session_id = parsed.session_id.clone();
                // Resolve auto-organization target (if the file is a flat
                // drop and we have a current target SHA + clean plan).
                let move_plan: Option<MovePlan> = if parsed.target_sha.is_none() {
                    self.resolve_flat_drop_target(repo_root, &parsed).await?
                } else {
                    None
                };

                if let Some(plan) = move_plan {
                    // Rename on disk + suppress the resulting watcher events.
                    if let Some(parent) = plan.to.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    self.mark_self_writes(&[abs_path.clone(), plan.to.clone()])
                        .await;
                    std::fs::rename(&abs_path, &plan.to)?;
                    let mut trinity = self.state.lock().await;
                    let state = trinity
                        .repos
                        .get_mut(repo_root)
                        .ok_or_else(|| RuntimeError::UnknownRepo(repo_root.to_path_buf()))?;
                    let Some(session) = state.sessions.get_mut(&session_id) else {
                        return Ok(());
                    };
                    upsert_feedback_at_target(
                        session,
                        plan.to.clone(),
                        plan.target_sha,
                        parsed.phase,
                        parsed.author.clone(),
                        body,
                    );
                    self.push_event(
                        &mut trinity,
                        LiveEvent {
                            ts: now,
                            repo: repo_root.to_path_buf(),
                            session_id: Some(session_id),
                            kind: "feedback_changed",
                            payload: serde_json::Value::Null,
                        },
                    );
                    return Ok(());
                }

                // No auto-organize move; ingest as-is (canonical path or held).
                let mut trinity = self.state.lock().await;
                let Some(state) = trinity.repos.get_mut(repo_root) else {
                    return Err(RuntimeError::UnknownRepo(repo_root.to_path_buf()));
                };
                let Some(session) = state.sessions.get_mut(&session_id) else {
                    return Ok(());
                };
                upsert_feedback(session, abs_path, parsed, body);
                self.push_event(
                    &mut trinity,
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
                let abs_path = repo_root
                    .join(".trinity/feedback")
                    .join(&parsed.raw);
                if self.should_skip_self_write(&abs_path).await {
                    return Ok(());
                }
                let mut trinity = self.state.lock().await;
                let Some(state) = trinity.repos.get_mut(repo_root) else {
                    return Err(RuntimeError::UnknownRepo(repo_root.to_path_buf()));
                };
                let session_id = parsed.session_id.clone();
                let Some(session) = state.sessions.get_mut(&session_id) else {
                    return Ok(());
                };
                remove_feedback(session, &parsed);
                self.push_event(
                    &mut trinity,
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

/// Result of resolving a flat-drop feedback file's auto-organize move.
#[derive(Debug)]
struct MovePlan {
    to: PathBuf,
    target_sha: CommitSha,
}

impl Runtime {
    /// Re-dispatch any currently-held flat-drop feedback after a rebuild.
    /// Each held entry is removed and forwarded back into `handle_signal`
    /// as a `FeedbackWritten`; the dispatch path will rename + ingest the
    /// file if the worktree is clean enough, or re-hold if it isn't.
    pub async fn normalize_held_feedback(
        &self,
        repo_root: &Path,
        now: i64,
    ) -> Result<(), RuntimeError> {
        let drained: Vec<(SessionId, HeldFeedback)> = {
            let mut trinity = self.state.lock().await;
            let Some(state) = trinity.repos.get_mut(repo_root) else {
                return Ok(());
            };
            let mut out = Vec::new();
            for (id, sess) in &mut state.sessions {
                let held: Vec<HeldFeedback> = std::mem::take(&mut sess.held_plan_feedback);
                for h in held {
                    out.push((id.clone(), h));
                }
            }
            out
        };

        let feedback_root = repo_root.join(".trinity/feedback");
        for (_session_id, held) in drained {
            let Ok(rel) = held.path.strip_prefix(&feedback_root) else {
                continue;
            };
            let Some(parsed) = crate::disk_format::parse_feedback_path(rel) else {
                continue;
            };
            // Only re-dispatch if the file is still on disk; if the user
            // deleted it between the previous run and now, drop the entry.
            if !held.path.exists() {
                continue;
            }
            // Forward as a FeedbackWritten; the dispatch will auto-organize
            // or hold according to the current worktree state.
            if let Err(err) = Box::pin(self.handle_signal(
                repo_root,
                FilesystemSignal::FeedbackWritten { parsed },
                now,
            ))
            .await
            {
                tracing::warn!(error = ?err, "normalize_held_feedback re-dispatch failed");
            }
        }
        Ok(())
    }

    /// Decide whether a flat-drop feedback file should be auto-organized
    /// into `<phase>/<target-sha>/<author>.md`.
    ///
    /// Returns `Some(MovePlan)` when a current target SHA exists and the
    /// session's plan is clean (for plan-phase) or always (for impl-phase).
    /// Returns `None` to fall back to the held-feedback path.
    async fn resolve_flat_drop_target(
        &self,
        repo_root: &Path,
        parsed: &crate::disk_format::FeedbackPath,
    ) -> Result<Option<MovePlan>, RuntimeError> {
        // Pull the data we need out of state under one short lock.
        let snapshot = {
            let trinity = self.state.lock().await;
            let Some(state) = trinity.repos.get(repo_root) else {
                return Ok(None);
            };
            let Some(session) = state.sessions.get(&parsed.session_id) else {
                return Ok(None);
            };
            FlatDropSnapshot {
                body_hash: session.body_hash.clone(),
                plan_path: session.plan_path.clone(),
                target: match parsed.phase {
                    FeedbackPhase::Plan => latest_attributed_commit(session, state, |attr| {
                        matches!(
                            attr,
                            AttributionResult::Attributed {
                                plan_touch: Some(_),
                                ..
                            }
                        )
                    }),
                    FeedbackPhase::Impl => latest_attributed_commit(session, state, |attr| {
                        matches!(
                            attr,
                            AttributionResult::Attributed {
                                has_code_changes: true,
                                ..
                            }
                        )
                    }),
                },
            }
        };

        let Some(target_sha) = snapshot.target else {
            return Ok(None);
        };

        // Plan-phase only: hold (don't auto-organize) when the worktree
        // is BodyDirty. Done-move-pending and missing-active-file mean
        // the plan path is intentionally absent or migrated — the
        // current target SHA from HEAD is still the right anchor for
        // an incoming review.
        if matches!(parsed.phase, FeedbackPhase::Plan) {
            use crate::projection::plan_worktree_status;
            use crate::repo_state::PlanWorktreeStatus;
            let active_path = repo_root.join(&snapshot.plan_path);
            let counterpart_rel =
                crate::mcp_response::swap_active_done(&snapshot.plan_path);
            let counterpart_abs = counterpart_rel.as_ref().map(|p| repo_root.join(p));
            let wt_hash = std::fs::read_to_string(&active_path)
                .ok()
                .map(|b| content_hash(&b));
            let counterpart_exists = counterpart_abs
                .as_ref()
                .map(|p| p.exists())
                .unwrap_or(false);
            let status = plan_worktree_status(
                Some(&snapshot.body_hash),
                wt_hash.as_ref(),
                counterpart_exists,
            );
            if matches!(status, PlanWorktreeStatus::BodyDirty) {
                return Ok(None);
            }
        }

        let to = repo_root
            .join(".trinity/feedback")
            .join(parsed.session_id.as_str())
            .join(parsed.phase.as_str())
            .join(target_sha.as_str())
            .join(format!("{}.md", parsed.author.as_str()));

        Ok(Some(MovePlan { to, target_sha }))
    }
}

struct FlatDropSnapshot {
    body_hash: crate::lifecycle::ContentHash,
    plan_path: PathBuf,
    target: Option<CommitSha>,
}

/// Latest commit attributed to `session.id` matching `pred`, in
/// chronological order from `RepoState::commit_order`. BTreeMap iteration
/// over `attribution` alone is SHA-lex order, not chronological, so we
/// walk `commit_order` and look up each entry.
fn latest_attributed_commit(
    session: &Session,
    state: &crate::repo_state::RepoState,
    pred: impl Fn(&AttributionResult) -> bool,
) -> Option<CommitSha> {
    let mut latest = None;
    for sha in &state.commit_order {
        let Some(attr) = state.attribution.get(sha) else {
            continue;
        };
        if let AttributionResult::Attributed { session: sid, .. } = attr
            && sid == &session.id
            && pred(attr)
        {
            latest = Some(sha.clone());
        }
    }
    latest
}

fn upsert_feedback_at_target(
    session: &mut Session,
    abs_path: PathBuf,
    target_sha: CommitSha,
    phase: FeedbackPhase,
    author: crate::lifecycle::AgentLabel,
    body: String,
) {
    let verdict = crate::disk_format::parse_verdict(&body);
    let created_at = file_mtime_unix_secs(&abs_path);
    let map = match phase {
        FeedbackPhase::Plan => &mut session.plan_feedback,
        FeedbackPhase::Impl => &mut session.impl_feedback,
    };
    map.insert(
        (target_sha, author),
        Feedback {
            path: abs_path,
            body,
            verdict,
            created_at,
        },
    );
}

/// File mtime as unix seconds, falling back to 0 if metadata is
/// unavailable. Used by feedback upserts so the in-memory `Feedback` /
/// `HeldFeedback` records carry the same chronological hint that the
/// initial-rebuild path picks up via `git_io::collect_feedback_files`.
fn file_mtime_unix_secs(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Runtime {
    /// Push a live event into the ring AND broadcast on the SSE channel.
    /// Caller must hold the state lock — this writes to `trinity.live_events`.
    fn push_event(&self, trinity: &mut Trinity, event: LiveEvent) {
        const CAPACITY: usize = 200;
        if trinity.live_events.len() >= CAPACITY {
            trinity.live_events.pop_front();
        }
        trinity.live_events.push_back(event.clone());
        // Broadcast is non-blocking; ignored if no receivers.
        let _ = self.events_tx.send(event);
    }
}

fn upsert_feedback(
    session: &mut Session,
    abs_path: PathBuf,
    parsed: crate::disk_format::FeedbackPath,
    body: String,
) {
    let verdict = crate::disk_format::parse_verdict(&body);
    let created_at = file_mtime_unix_secs(&abs_path);
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
                    created_at,
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
                created_at,
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
        let snapshot = rt
            .snapshot_session(dir.path(), &SessionId::from("foo".to_string()))
            .await
            .unwrap()
            .unwrap();
        let v = get_context_response(&snapshot, &AgentLabel::from("master".to_string())).unwrap();
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

        let snapshot = rt
            .snapshot_session(dir.path(), &SessionId::from("foo".to_string()))
            .await
            .unwrap()
            .unwrap();
        let v = get_context_response(&snapshot, &AgentLabel::from("master".to_string())).unwrap();
        // Gate falls back to no participants → reviewers / plan_needs_initial_review.
        assert_eq!(v["waiting_on"]["reason"], "plan_needs_initial_review");
    }

    #[tokio::test]
    async fn flat_drop_feedback_is_auto_organized() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        // Drop a flat feedback file (no sha sub-dir).
        let feedback_rel = ".trinity/feedback/foo/plan/alice.md";
        write_file(dir.path(), feedback_rel, "APPROVE\n");

        let parsed_rel = PathBuf::from("foo/plan/alice.md");
        let parsed = crate::disk_format::parse_feedback_path(&parsed_rel).unwrap();
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten { parsed },
            7,
        )
        .await
        .unwrap();

        // The flat file should be gone; the canonical SHA-subdir file should exist.
        let intro_sha = rt
            .read_repo(dir.path(), |s| {
                s.sessions[&SessionId::from("foo".to_string())]
                    .plan_intro
                    .clone()
            })
            .await
            .unwrap();
        let canonical_rel = format!(
            ".trinity/feedback/foo/plan/{}/alice.md",
            intro_sha.as_str()
        );
        assert!(
            !dir.path().join(feedback_rel).exists(),
            "flat-drop file should be gone"
        );
        assert!(
            dir.path().join(&canonical_rel).exists(),
            "canonical file should exist at {canonical_rel}"
        );

        // The session's plan_feedback map should have the entry under
        // the target SHA + alice key.
        let key_present = rt
            .read_repo(dir.path(), |s| {
                s.sessions[&SessionId::from("foo".to_string())]
                    .plan_feedback
                    .contains_key(&(intro_sha, AgentLabel::from("alice".to_string())))
            })
            .await
            .unwrap();
        assert!(key_present);
    }

    #[tokio::test]
    async fn flat_drop_held_when_plan_is_body_dirty() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "add foo");
        // Edit the plan without committing
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2 uncommitted\n");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let feedback_rel = ".trinity/feedback/foo/plan/alice.md";
        write_file(dir.path(), feedback_rel, "APPROVE\n");

        let parsed_rel = PathBuf::from("foo/plan/alice.md");
        let parsed = crate::disk_format::parse_feedback_path(&parsed_rel).unwrap();
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten { parsed },
            1,
        )
        .await
        .unwrap();

        // Flat file should still be there (held, not renamed).
        assert!(
            dir.path().join(feedback_rel).exists(),
            "flat file should still exist while plan is body_dirty"
        );
        let held_count = rt
            .read_repo(dir.path(), |s| {
                s.sessions[&SessionId::from("foo".to_string())]
                    .held_plan_feedback
                    .len()
            })
            .await
            .unwrap();
        assert_eq!(held_count, 1);
    }

    #[tokio::test]
    async fn held_feedback_releases_after_plan_commit() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "add foo v1");
        // Edit plan without committing → body_dirty
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2 uncommitted\n");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        // Drop a flat feedback file while plan is dirty → held.
        let feedback_rel = ".trinity/feedback/foo/plan/alice.md";
        write_file(dir.path(), feedback_rel, "APPROVE\n");
        let parsed_rel = PathBuf::from("foo/plan/alice.md");
        let parsed = crate::disk_format::parse_feedback_path(&parsed_rel).unwrap();
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten { parsed },
            1,
        )
        .await
        .unwrap();

        let held_count = rt
            .read_repo(dir.path(), |s| {
                s.sessions[&SessionId::from("foo".to_string())]
                    .held_plan_feedback
                    .len()
            })
            .await
            .unwrap();
        assert_eq!(held_count, 1, "should be held while dirty");

        // Commit the plan revision so it becomes clean.
        commit(dir.path(), "revise foo to v2");

        // HEAD changed → rebuild + normalize → held flat file should
        // auto-organize into the new target SHA subdir.
        rt.handle_signal(dir.path(), FilesystemSignal::HeadChanged, 2)
            .await
            .unwrap();

        let (held_after, has_canonical_entry) = rt
            .read_repo(dir.path(), |s| {
                let sess = &s.sessions[&SessionId::from("foo".to_string())];
                let any_entry_for_alice = sess
                    .plan_feedback
                    .keys()
                    .any(|(_, author)| author.as_str() == "alice");
                (sess.held_plan_feedback.len(), any_entry_for_alice)
            })
            .await
            .unwrap();
        assert_eq!(held_after, 0, "held should clear after rebuild");
        assert!(
            has_canonical_entry,
            "alice's feedback should be in plan_feedback under the new target sha"
        );
        // Flat file should be gone from disk.
        assert!(
            !dir.path().join(feedback_rel).exists(),
            "flat file should have been moved"
        );
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

        let snapshot = rt.snapshot_repo(dir.path()).await.unwrap();
        let v = list_sessions_response(&snapshot).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[tokio::test]
    async fn broadcast_delivers_live_events_to_subscribers() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "Add foo");

        let rt = Runtime::new();
        let mut rx = rt.subscribe_events();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        // Trigger a rebuild by signaling HeadChanged.
        write_file(dir.path(), "src/x.rs", "fn x() {}\n");
        commit(dir.path(), "second commit");
        rt.handle_signal(dir.path(), FilesystemSignal::HeadChanged, 1)
            .await
            .unwrap();

        // We should receive a `repo_rebuilt` event on the broadcast.
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for broadcast")
            .expect("broadcast closed");
        assert_eq!(event.kind, "repo_rebuilt");
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
