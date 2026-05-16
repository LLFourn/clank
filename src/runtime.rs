//! Trinity runtime: owns the in-memory `Trinity` state and handles
//! filesystem signals coming in from the watcher layer.
//!
//! This module is the bridge between the pure core (rebuild + reducer)
//! and the rest of the daemon. The notify wiring (separate, not yet
//! written) feeds `FilesystemSignal` values into `handle_signal`;
//! request handlers (MCP, HTTP) read state via owned snapshots.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, broadcast};

use crate::fs_watcher::FilesystemSignal;
use crate::lifecycle::{CommitSha, PlanKey, content_hash};
use crate::rebuild::{RebuildError, rebuild_repo};
use crate::repo_state::RepoState;
use crate::repo_state::{Feedback, LiveEvent, Plan, Trinity};

pub struct Runtime {
    state: Arc<Mutex<Trinity>>,
    /// Broadcast channel for live events. SSE handlers subscribe here.
    events_tx: broadcast::Sender<LiveEvent>,
}

impl Default for Runtime {
    fn default() -> Self {
        let (events_tx, _) = broadcast::channel(256);
        Self {
            state: Arc::default(),
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
    #[error("repo path has no usable basename: {0}")]
    InvalidRepoPath(PathBuf),
}

/// Outcome of `Runtime::add_repo`. The shadowed-by-other case is not an
/// error — daemon startup keeps booting and request handlers fall back
/// to whatever the first-registered repo at that basename serves — but
/// callers MUST be able to distinguish it from `Registered` so they can
/// log accurately and (for `start_plan`) refuse mutating disk on a
/// shadowed repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    Registered,
    ShadowedByOther { claimed_by: PathBuf },
}

/// Outcome of `Runtime::remove_repo`. `NotPresent` lets the HTTP
/// handler return 404 without needing to inspect the runtime state
/// before the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed {
        /// Number of plans dropped along with the repo. Used by the
        /// `repo_unwatched` LiveEvent payload so the SPA can show a
        /// "removed N plans" toast if it wants.
        plan_count: usize,
    },
    NotPresent,
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
    pub async fn add_repo(&self, repo_root: PathBuf) -> Result<RegisterOutcome, RuntimeError> {
        let canonical = dunce::canonicalize(&repo_root).unwrap_or(repo_root);
        let basename = crate::lifecycle::RepoBasename::from_repo_root(&canonical)
            .ok_or_else(|| RuntimeError::InvalidRepoPath(canonical.clone()))?;
        let fresh = rebuild_repo(&canonical).await?;
        {
            let mut trinity = self.state.lock().await;
            // Basename collision: first registration wins. The shadowed
            // repo is reported back so the caller (server startup, MCP
            // start_plan) can log accurately and refuse work against it.
            if let Some(claimed_by) = trinity.repo_basenames.get(&basename)
                && claimed_by != &canonical
            {
                let claimed_by = claimed_by.clone();
                tracing::warn!(
                    basename = %basename,
                    claimed_by = %claimed_by.display(),
                    shadowed = %canonical.display(),
                    "repo basename collides with an already-watched repo; shadowed repo will be ignored"
                );
                return Ok(RegisterOutcome::ShadowedByOther { claimed_by });
            }
            trinity.repos.insert(canonical.clone(), fresh);
            trinity.repo_basenames.insert(basename, canonical.clone());
        }
        Ok(RegisterOutcome::Registered)
    }

    /// Register a repo only if it isn't already known. Returns the
    /// outcome so callers can distinguish a successful registration
    /// (or a no-op against an already-known repo) from a shadowing
    /// collision. Rebuild errors are swallowed (logged) and surface as
    /// `Err`; callers like request handlers usually ignore them.
    pub async fn add_repo_if_unknown(
        &self,
        repo_root: PathBuf,
    ) -> Result<RegisterOutcome, RuntimeError> {
        let canonical = dunce::canonicalize(&repo_root).unwrap_or(repo_root);
        {
            let trinity = self.state.lock().await;
            if trinity.repos.contains_key(&canonical) {
                return Ok(RegisterOutcome::Registered);
            }
        }
        self.add_repo(canonical).await
    }

    /// Deregister `repo_root` from in-memory state. Drops the repo from
    /// `Trinity.repos` and any matching entries in `Trinity.repo_basenames`.
    /// Does NOT touch the watcher set (the caller — the HTTP handler —
    /// owns watcher-handle cleanup) and does NOT touch the on-disk
    /// registry (the caller writes that too). The `HeadChanged`
    /// resurrection guard in `handle_signal` ensures any in-flight
    /// watcher signal can't re-insert the entry.
    pub async fn remove_repo(&self, repo_root: PathBuf) -> RemoveOutcome {
        let canonical = dunce::canonicalize(&repo_root).unwrap_or(repo_root);
        let mut trinity = self.state.lock().await;
        let Some(removed) = trinity.repos.remove(&canonical) else {
            return RemoveOutcome::NotPresent;
        };
        let plan_count = removed.plans.len();
        trinity.repo_basenames.retain(|_, root| root != &canonical);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.push_event(
            &mut trinity,
            LiveEvent {
                ts: now,
                repo: canonical,
                plan_id: None,
                state: None,
                kind: "repo_unwatched",
                payload: serde_json::json!({ "plan_count": plan_count }),
            },
        );
        RemoveOutcome::Removed { plan_count }
    }

    /// Clone the repo's `RepoState` while holding the runtime lock.
    /// Callers do disk and git I/O after this returns.
    pub async fn snapshot_repo(&self, repo_root: &Path) -> Result<RepoState, RuntimeError> {
        let canonical = dunce::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
        let trinity = self.state.lock().await;
        let state = trinity
            .repos
            .get(&canonical)
            .ok_or_else(|| RuntimeError::UnknownRepo(canonical.clone()))?;
        Ok(state.clone())
    }

    /// Clone a single-plan slice of `RepoState`: just the requested
    /// plan in `plans`, with `plan_conflicts` cleared. Repo-level
    /// indexes (`attribution`, `plan_touches`, `commit_order`,
    /// `commit_meta`) carry through. `Ok(None)` means the repo is
    /// known but the session is not committed.
    pub async fn snapshot_session(
        &self,
        repo_root: &Path,
        session_id: &PlanKey,
    ) -> Result<Option<RepoState>, RuntimeError> {
        let canonical = dunce::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
        let trinity = self.state.lock().await;
        let state = trinity
            .repos
            .get(&canonical)
            .ok_or_else(|| RuntimeError::UnknownRepo(canonical.clone()))?;
        Ok(state.single_plan(session_id))
    }

    /// Read-only in-memory access for tests. Do not call disk-aware
    /// helpers from the closure.
    #[cfg(test)]
    pub(crate) async fn read_repo<R>(
        &self,
        repo_root: &Path,
        f: impl FnOnce(&crate::repo_state::RepoState) -> R,
    ) -> Result<R, RuntimeError> {
        let canonical = dunce::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
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
        let repo_root_canonical =
            dunce::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
        let repo_root = repo_root_canonical.as_path();
        match signal {
            FilesystemSignal::HeadChanged => {
                // Resurrection guard: if the repo was just unwatched
                // (via `remove_repo` or daemon-startup skip), a late /
                // debounced HeadChanged from the now-aborted watcher
                // would re-insert it. Skip the rebuild + insert when
                // the repo isn't already tracked. PlanFileChanged /
                // FeedbackWritten / FeedbackRemoved get the same effect
                // implicitly: each does `trinity.repos.get(repo_root)?`
                // and bails when the entry is gone.
                {
                    let trinity = self.state.lock().await;
                    if !trinity.repos.contains_key(repo_root) {
                        return Ok(());
                    }
                }
                let fresh = rebuild_repo(repo_root).await?;
                let fresh_digest = fresh.digest();
                {
                    let mut trinity = self.state.lock().await;
                    // Re-check under the second lock acquire: removal
                    // could have raced our rebuild.
                    if !trinity.repos.contains_key(repo_root) {
                        return Ok(());
                    }
                    let prior_digest = trinity.repos.get(repo_root).map(|r| r.digest());
                    let changed = prior_digest.as_ref() != Some(&fresh_digest);
                    trinity.repos.insert(repo_root.to_path_buf(), fresh);
                    if changed {
                        self.push_event(
                            &mut trinity,
                            LiveEvent {
                                ts: now,
                                repo: repo_root.to_path_buf(),
                                plan_id: None,
                                state: None,
                                kind: "repo_rebuilt",
                                payload: serde_json::Value::Null,
                            },
                        );
                    }
                }
            }
            FilesystemSignal::PlanFileChanged { session_id, path } => {
                let mut trinity = self.state.lock().await;
                let Some(state) = trinity.repos.get(repo_root) else {
                    return Err(RuntimeError::UnknownRepo(repo_root.to_path_buf()));
                };
                let Some(plan) = state.plans.get(&session_id) else {
                    return Ok(());
                };
                let plan_state = plan.state;
                let plan_id = plan_id_for(repo_root, &plan.id);
                self.push_event(
                    &mut trinity,
                    LiveEvent {
                        ts: now,
                        repo: repo_root.to_path_buf(),
                        plan_id,
                        state: Some(plan_state),
                        kind: "plan_worktree_changed",
                        payload: serde_json::json!({"path": path.to_string_lossy()}),
                    },
                );
            }
            FilesystemSignal::FeedbackWritten { parsed } => {
                let abs_path = repo_root.join(".trinity/feedback").join(&parsed.raw);
                let body = match std::fs::read_to_string(&abs_path) {
                    Ok(b) => b,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(e) => return Err(e.into()),
                };
                let session_id = parsed.plan_key.clone();
                let mut trinity = self.state.lock().await;
                let Some(state) = trinity.repos.get_mut(repo_root) else {
                    return Err(RuntimeError::UnknownRepo(repo_root.to_path_buf()));
                };
                let Some(session) = state.plans.get_mut(&session_id) else {
                    return Ok(());
                };
                upsert_feedback(session, abs_path, parsed, body);
                refresh_commits_for(state, &session_id);
                let plan_state = state.plans.get(&session_id).map(|p| p.state);
                let plan_id = plan_id_for(repo_root, &session_id);
                self.push_event(
                    &mut trinity,
                    LiveEvent {
                        ts: now,
                        repo: repo_root.to_path_buf(),
                        plan_id,
                        state: plan_state,
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
                let session_id = parsed.plan_key.clone();
                let Some(session) = state.plans.get_mut(&session_id) else {
                    return Ok(());
                };
                remove_feedback(session, &parsed);
                refresh_commits_for(state, &session_id);
                let plan_state = state.plans.get(&session_id).map(|p| p.state);
                let plan_id = plan_id_for(repo_root, &session_id);
                self.push_event(
                    &mut trinity,
                    LiveEvent {
                        ts: now,
                        repo: repo_root.to_path_buf(),
                        plan_id,
                        state: plan_state,
                        kind: "feedback_removed",
                        payload: serde_json::Value::Null,
                    },
                );
            }
        }
        Ok(())
    }
}

/// Construct a `PlanId` from a repo root + plan key. Returns `None` if
/// the repo path lacks a usable file_name (shouldn't happen for any path
/// the daemon actually canonicalized via `dunce`).
fn plan_id_for(
    repo_root: &Path,
    plan_key: &crate::lifecycle::PlanKey,
) -> Option<crate::lifecycle::PlanId> {
    let basename = crate::lifecycle::RepoBasename::from_repo_root(repo_root)?;
    Some(crate::lifecycle::PlanId::new(basename, plan_key.clone()))
}

// File mtime helper lives next to its sibling rebuild-path call site
// in `git_io`. See `git_io::file_mtime_unix_secs`.
use crate::git_io::file_mtime_unix_secs;

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
    session: &mut Plan,
    abs_path: PathBuf,
    parsed: crate::disk_format::FeedbackPath,
    body: String,
) {
    let verdict = crate::disk_format::parse_verdict(&body);
    let created_at = file_mtime_unix_secs(&abs_path);
    let entry = session.commits.entry(parsed.target_sha).or_insert_with(|| {
        crate::review_state::CommitGate {
            state: crate::review_state::CommitGateState::Unreviewed,
            participants: Vec::new(),
            approvers: Vec::new(),
            requesters: Vec::new(),
            ambiguous: Vec::new(),
            missing: Vec::new(),
            feedback: std::collections::BTreeMap::new(),
        }
    });
    entry.feedback.insert(
        parsed.author,
        Feedback {
            path: abs_path,
            body,
            verdict,
            created_at,
        },
    );
}

fn remove_feedback(session: &mut Plan, parsed: &crate::disk_format::FeedbackPath) {
    if let Some(gate) = session.commits.get_mut(&parsed.target_sha) {
        gate.feedback.remove(&parsed.author);
    }
}

/// Flatten all SHA-targeted feedback from `Plan.commits[*].feedback`
/// into a single `(sha, author) → Feedback` map. Used as the input
/// to `build_commit_gates` when re-folding after a runtime signal
/// mutates a single entry.
fn extract_feedback(plan: &Plan) -> BTreeMap<(CommitSha, crate::lifecycle::AgentLabel), Feedback> {
    let mut out = BTreeMap::new();
    for (sha, gate) in &plan.commits {
        for (author, fb) in &gate.feedback {
            out.insert((sha.clone(), author.clone()), fb.clone());
        }
    }
    out
}

/// Rebuild `Plan.commits` for one plan from its own current
/// `commits[*].feedback`. Called from the signal handlers after every
/// mutation so cumulative-participant state, gate roll-up, and
/// non-reviewable filtering all re-fold consistently between full
/// rebuilds.
fn refresh_commits_for(state: &mut crate::repo_state::RepoState, plan_key: &PlanKey) {
    let Some(plan) = state.plans.get(plan_key) else {
        return;
    };
    let feedback = extract_feedback(plan);
    let new_commits = crate::projection::build_commit_gates(
        plan_key,
        &state.commit_order,
        &state.plan_touches,
        &state.attribution,
        &feedback,
    );
    if let Some(plan) = state.plans.get_mut(plan_key) {
        plan.commits = new_commits;
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
    use crate::lifecycle::{AgentLabel, CommitSha, PlanKey};
    use crate::mcp_response::{get_context_response, list_plans_response};
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
    async fn remove_repo_drops_state_and_basename_index() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();
        let canonical = dunce::canonicalize(dir.path()).unwrap();
        let outcome = rt.remove_repo(canonical.clone()).await;
        assert_eq!(outcome, RemoveOutcome::Removed { plan_count: 1 });

        let trinity = rt.state().lock_owned().await;
        assert!(!trinity.repos.contains_key(&canonical));
        let basename = crate::lifecycle::RepoBasename::from_repo_root(&canonical).unwrap();
        assert!(!trinity.repo_basenames.contains_key(&basename));
    }

    #[tokio::test]
    async fn remove_repo_returns_not_present_when_absent() {
        let rt = Runtime::new();
        let outcome = rt.remove_repo(std::path::PathBuf::from("/nowhere")).await;
        assert_eq!(outcome, RemoveOutcome::NotPresent);
    }

    #[tokio::test]
    async fn head_changed_does_not_resurrect_removed_repo() {
        // Late watcher signal arriving after `remove_repo` must NOT
        // re-insert the repo. Without the guard, the prior code path
        // unconditionally inserted the rebuild result, so an unwatch
        // would silently undo itself the next time the watcher
        // debounced.
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();
        let canonical = dunce::canonicalize(dir.path()).unwrap();
        rt.remove_repo(canonical.clone()).await;

        // Drain any `repo_unwatched` event from the buffer before the
        // signal fires so we can assert no `repo_rebuilt` slips in.
        let before = rt.live_events_snapshot().await.len();

        rt.handle_signal(
            &canonical,
            crate::fs_watcher::FilesystemSignal::HeadChanged,
            42,
        )
        .await
        .unwrap();

        let trinity = rt.state().lock_owned().await;
        assert!(
            !trinity.repos.contains_key(&canonical),
            "removed repo must not resurrect on HeadChanged"
        );
        let after_events = trinity.live_events.clone();
        drop(trinity);
        let new_events: Vec<_> = after_events.iter().skip(before).collect();
        assert!(
            !new_events.iter().any(|e| e.kind == "repo_rebuilt"),
            "no repo_rebuilt should fire for an unwatched repo; got events: {new_events:?}"
        );
    }

    #[tokio::test]
    async fn add_repo_loads_initial_state() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let count = rt.read_repo(dir.path(), |s| s.plans.len()).await.unwrap();
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
                session_id: PlanKey::parse("foo").unwrap(),
                path: PathBuf::from(".trinity/plans/foo.md"),
            },
            42,
        )
        .await
        .unwrap();

        let events = rt.live_events_snapshot().await;
        assert!(events.iter().any(|e| {
            e.kind == "plan_worktree_changed"
                && e.plan_id.as_ref().map(|id| id.key().as_str()) == Some("foo")
        }));
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
                session_id: PlanKey::parse("unknown").unwrap(),
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
                s.plans[&PlanKey::parse("foo").unwrap()].plan_intro.clone()
            })
            .await
            .unwrap();

        // Write feedback file on disk first.
        let feedback_rel = format!(".trinity/feedback/foo/commits/{}/alice.md", intro.as_str());
        write_file(dir.path(), &feedback_rel, "APPROVE\n\nlgtm\n");

        // Build the parsed FeedbackPath manually for the signal.
        let parsed_rel = PathBuf::from(format!("foo/commits/{}/alice.md", intro.as_str()));
        let parsed = crate::disk_format::parse_feedback_path(&parsed_rel).unwrap();

        rt.handle_signal(dir.path(), FilesystemSignal::FeedbackWritten { parsed }, 7)
            .await
            .unwrap();

        // The gate should now resolve to ready_to_implement.
        let snapshot = rt
            .snapshot_session(dir.path(), &PlanKey::parse("foo").unwrap())
            .await
            .unwrap()
            .unwrap();
        let v = get_context_response(&snapshot, &AgentLabel::parse("master").unwrap()).unwrap();
        assert_eq!(v["waiting_on"]["reason"], "ready_to_start_implementation");

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
                s.plans[&PlanKey::parse("foo").unwrap()].plan_intro.clone()
            })
            .await
            .unwrap();
        let feedback_rel = format!(".trinity/feedback/foo/commits/{}/alice.md", intro.as_str());
        write_file(dir.path(), &feedback_rel, "APPROVE\n");
        let parsed_rel = PathBuf::from(format!("foo/commits/{}/alice.md", intro.as_str()));
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
        rt.handle_signal(dir.path(), FilesystemSignal::FeedbackRemoved { parsed }, 2)
            .await
            .unwrap();

        let snapshot = rt
            .snapshot_session(dir.path(), &PlanKey::parse("foo").unwrap())
            .await
            .unwrap()
            .unwrap();
        let v = get_context_response(&snapshot, &AgentLabel::parse("master").unwrap()).unwrap();
        // Gate falls back to no participants → reviewers / plan_needs_initial_review.
        assert_eq!(v["waiting_on"]["reason"], "commit_needs_review");
    }

    // (flat_drop_feedback_is_auto_organized test removed in phase 2.4 —
    // the flat-drop disk path / held-feedback / auto-organize path
    // no longer exists; reviewers always write to commits/<sha>/.)

    #[tokio::test]
    async fn list_plans_via_runtime() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        write_file(dir.path(), ".trinity/plans/bar.md", "# bar\n");
        commit(dir.path(), "add bar");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let snapshot = rt.snapshot_repo(dir.path()).await.unwrap();
        let v = list_plans_response(&snapshot).unwrap();
        let arr = v["plans"].as_array().unwrap();
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
                    session_id: PlanKey::parse("foo").unwrap(),
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
