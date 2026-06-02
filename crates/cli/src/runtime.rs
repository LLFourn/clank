//! Clank runtime: owns the in-memory `Clank` state and handles
//! filesystem signals from the watcher layer.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, broadcast};

use crate::fs_watcher::FilesystemSignal;
use crate::lifecycle::PlanKey;
use crate::rebuild::{RebuildError, rebuild_repo};
use crate::repo_state::{Clank, LiveEvent, PlanEvent, RepoEvent, RepoState};
use clank_core::api::{PlanEventPayload, RepoEventPayload};

pub struct Runtime {
    state: Arc<Mutex<Clank>>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    Registered,
    ShadowedByOther { claimed_by: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed { plan_count: usize },
    NotPresent,
}

impl Runtime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> Arc<Mutex<Clank>> {
        Arc::clone(&self.state)
    }

    pub async fn add_repo(&self, repo_root: PathBuf) -> Result<RegisterOutcome, RuntimeError> {
        let canonical = dunce::canonicalize(&repo_root).unwrap_or(repo_root);
        let basename = crate::lifecycle::RepoBasename::from_repo_root(&canonical)
            .ok_or_else(|| RuntimeError::InvalidRepoPath(canonical.clone()))?;
        let fresh = rebuild_repo(&canonical).await?;
        {
            let mut clank = self.state.lock().await;
            if let Some(claimed_by) = clank.repo_basenames.get(&basename)
                && claimed_by != &canonical
            {
                let claimed_by = claimed_by.clone();
                tracing::warn!(
                    basename = %basename,
                    claimed_by = %claimed_by.display(),
                    shadowed = %canonical.display(),
                    "repo basename collides with an already-watched repo"
                );
                return Ok(RegisterOutcome::ShadowedByOther { claimed_by });
            }
            clank.repos.insert(canonical.clone(), fresh);
            clank.repo_basenames.insert(basename, canonical.clone());
        }
        Ok(RegisterOutcome::Registered)
    }

    pub async fn add_repo_if_unknown(
        &self,
        repo_root: PathBuf,
    ) -> Result<RegisterOutcome, RuntimeError> {
        let canonical = dunce::canonicalize(&repo_root).unwrap_or(repo_root);
        {
            let clank = self.state.lock().await;
            if clank.repos.contains_key(&canonical) {
                return Ok(RegisterOutcome::Registered);
            }
        }
        self.add_repo(canonical).await
    }

    pub async fn remove_repo(&self, repo_root: PathBuf) -> RemoveOutcome {
        let canonical = dunce::canonicalize(&repo_root).unwrap_or(repo_root);
        let mut clank = self.state.lock().await;
        let Some(removed) = clank.repos.remove(&canonical) else {
            return RemoveOutcome::NotPresent;
        };
        let plan_count = removed.fold.plans.len();
        clank.repo_basenames.retain(|_, root| root != &canonical);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.push_event(
            &mut clank,
            LiveEvent::Repo(RepoEvent {
                ts: now,
                repo: canonical,
                payload: RepoEventPayload::RepoUnwatched { plan_count },
            }),
        );
        RemoveOutcome::Removed { plan_count }
    }

    pub async fn snapshot_repo(&self, repo_root: &Path) -> Result<RepoState, RuntimeError> {
        let canonical = dunce::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
        let clank = self.state.lock().await;
        let state = clank
            .repos
            .get(&canonical)
            .ok_or_else(|| RuntimeError::UnknownRepo(canonical.clone()))?;
        Ok(state.clone())
    }

    pub async fn live_events_snapshot(&self) -> VecDeque<LiveEvent> {
        let clank = self.state.lock().await;
        clank.live_events.clone()
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<LiveEvent> {
        self.events_tx.subscribe()
    }

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
                {
                    let clank = self.state.lock().await;
                    if !clank.repos.contains_key(repo_root) {
                        return Ok(());
                    }
                }
                let fresh = rebuild_repo(repo_root).await?;
                let fresh_digest = fresh.digest();
                {
                    let mut clank = self.state.lock().await;
                    if !clank.repos.contains_key(repo_root) {
                        return Ok(());
                    }
                    let prior_digest = clank.repos.get(repo_root).map(|r| r.digest());
                    let changed = prior_digest.as_ref() != Some(&fresh_digest);
                    clank.repos.insert(repo_root.to_path_buf(), fresh);
                    if changed {
                        self.push_event(
                            &mut clank,
                            LiveEvent::Repo(RepoEvent {
                                ts: now,
                                repo: repo_root.to_path_buf(),
                                payload: RepoEventPayload::RepoRebuilt {},
                            }),
                        );
                    }
                }
            }
            FilesystemSignal::PlanFileChanged { session_id, path } => {
                self.emit_plan_event(
                    repo_root,
                    &session_id,
                    now,
                    PlanEventPayload::PlanWorktreeChanged {
                        path: path.to_string_lossy().into_owned(),
                    },
                )
                .await;
            }
            FilesystemSignal::FeedbackWritten { .. } | FilesystemSignal::FeedbackRemoved { .. } => {
                {
                    let clank = self.state.lock().await;
                    if !clank.repos.contains_key(repo_root) {
                        return Ok(());
                    }
                }
                let fresh = rebuild_repo(repo_root).await?;
                let fresh_digest = fresh.digest();
                {
                    let mut clank = self.state.lock().await;
                    if !clank.repos.contains_key(repo_root) {
                        return Ok(());
                    }
                    let prior_digest = clank.repos.get(repo_root).map(|r| r.digest());
                    let changed = prior_digest.as_ref() != Some(&fresh_digest);
                    clank.repos.insert(repo_root.to_path_buf(), fresh);
                    if changed {
                        self.push_event(
                            &mut clank,
                            LiveEvent::Repo(RepoEvent {
                                ts: now,
                                repo: repo_root.to_path_buf(),
                                payload: RepoEventPayload::RepoRebuilt {},
                            }),
                        );
                    }
                }
            }
        }
        Ok(())
    }

    async fn emit_plan_event(
        &self,
        repo_root: &Path,
        session_id: &PlanKey,
        now: i64,
        payload: PlanEventPayload,
    ) {
        let mut clank = self.state.lock().await;
        let Some(state) = clank.repos.get(repo_root) else {
            return;
        };
        let lifecycle = if state
            .fold
            .finished_plans
            .iter()
            .any(|f| &f.plan == session_id)
        {
            clank_core::PlanLifecycle::Finished
        } else if state.fold.plans.contains_key(session_id) {
            clank_core::PlanLifecycle::Active
        } else {
            return;
        };
        let Some(plan_id) = plan_id_for(repo_root, session_id) else {
            return;
        };
        self.push_event(
            &mut clank,
            LiveEvent::Plan(PlanEvent {
                ts: now,
                repo: repo_root.to_path_buf(),
                plan_id,
                lifecycle,
                payload,
            }),
        );
    }

    fn push_event(&self, clank: &mut Clank, event: LiveEvent) {
        const RING_CAP: usize = 256;
        if clank.live_events.len() >= RING_CAP {
            clank.live_events.pop_front();
        }
        clank.live_events.push_back(event.clone());
        let _ = self.events_tx.send(event);
    }
}

fn plan_id_for(repo_root: &Path, plan_key: &PlanKey) -> Option<crate::lifecycle::PlanId> {
    let basename = crate::lifecycle::RepoBasename::from_repo_root(repo_root)?;
    Some(crate::lifecycle::PlanId::new(basename, plan_key.clone()))
}
