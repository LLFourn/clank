//! Trinity runtime: owns the in-memory `Trinity` state and handles
//! filesystem signals from the watcher layer.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, broadcast};

use crate::fs_watcher::FilesystemSignal;
use crate::lifecycle::PlanKey;
use crate::rebuild::{RebuildError, rebuild_repo};
use crate::repo_state::{LiveEvent, PlanEvent, RepoEvent, RepoState, Trinity};
use trinity_core::api::{PlanEventPayload, RepoEventPayload};

pub struct Runtime {
    state: Arc<Mutex<Trinity>>,
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

    pub fn state(&self) -> Arc<Mutex<Trinity>> {
        Arc::clone(&self.state)
    }

    pub async fn add_repo(&self, repo_root: PathBuf) -> Result<RegisterOutcome, RuntimeError> {
        let canonical = dunce::canonicalize(&repo_root).unwrap_or(repo_root);
        let basename = crate::lifecycle::RepoBasename::from_repo_root(&canonical)
            .ok_or_else(|| RuntimeError::InvalidRepoPath(canonical.clone()))?;
        let fresh = rebuild_repo(&canonical).await?;
        {
            let mut trinity = self.state.lock().await;
            if let Some(claimed_by) = trinity.repo_basenames.get(&basename)
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
            trinity.repos.insert(canonical.clone(), fresh);
            trinity.repo_basenames.insert(basename, canonical.clone());
        }
        Ok(RegisterOutcome::Registered)
    }

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

    pub async fn remove_repo(&self, repo_root: PathBuf) -> RemoveOutcome {
        let canonical = dunce::canonicalize(&repo_root).unwrap_or(repo_root);
        let mut trinity = self.state.lock().await;
        let Some(removed) = trinity.repos.remove(&canonical) else {
            return RemoveOutcome::NotPresent;
        };
        let plan_count = removed.fold.plans.len();
        trinity.repo_basenames.retain(|_, root| root != &canonical);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.push_event(
            &mut trinity,
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
        let trinity = self.state.lock().await;
        let state = trinity
            .repos
            .get(&canonical)
            .ok_or_else(|| RuntimeError::UnknownRepo(canonical.clone()))?;
        Ok(state.clone())
    }

    pub async fn live_events_snapshot(&self) -> VecDeque<LiveEvent> {
        let trinity = self.state.lock().await;
        trinity.live_events.clone()
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
                    let trinity = self.state.lock().await;
                    if !trinity.repos.contains_key(repo_root) {
                        return Ok(());
                    }
                }
                let fresh = rebuild_repo(repo_root).await?;
                let fresh_digest = fresh.digest();
                {
                    let mut trinity = self.state.lock().await;
                    if !trinity.repos.contains_key(repo_root) {
                        return Ok(());
                    }
                    let prior_digest = trinity.repos.get(repo_root).map(|r| r.digest());
                    let changed = prior_digest.as_ref() != Some(&fresh_digest);
                    trinity.repos.insert(repo_root.to_path_buf(), fresh);
                    if changed {
                        self.push_event(
                            &mut trinity,
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
            FilesystemSignal::FeedbackWritten { parsed }
            | FilesystemSignal::FeedbackRemoved { parsed } => {
                let session_id = match parsed.target.clone() {
                    crate::disk_format::FeedbackTarget::Plan(k) => k,
                    crate::disk_format::FeedbackTarget::AdHoc => return Ok(()),
                };
                self.emit_plan_event(
                    repo_root,
                    &session_id,
                    now,
                    PlanEventPayload::FeedbackChanged {},
                )
                .await;
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
        let mut trinity = self.state.lock().await;
        let Some(state) = trinity.repos.get(repo_root) else {
            return;
        };
        let lifecycle = if state
            .fold
            .finished_plans
            .iter()
            .any(|f| &f.plan == session_id)
        {
            trinity_core::PlanLifecycle::Finished
        } else if state.fold.plans.contains_key(session_id) {
            trinity_core::PlanLifecycle::Active
        } else {
            return;
        };
        let Some(plan_id) = plan_id_for(repo_root, session_id) else {
            return;
        };
        self.push_event(
            &mut trinity,
            LiveEvent::Plan(PlanEvent {
                ts: now,
                repo: repo_root.to_path_buf(),
                plan_id,
                lifecycle,
                payload,
            }),
        );
    }

    fn push_event(&self, trinity: &mut Trinity, event: LiveEvent) {
        const RING_CAP: usize = 256;
        if trinity.live_events.len() >= RING_CAP {
            trinity.live_events.pop_front();
        }
        trinity.live_events.push_back(event.clone());
        let _ = self.events_tx.send(event);
    }
}

fn plan_id_for(repo_root: &Path, plan_key: &PlanKey) -> Option<crate::lifecycle::PlanId> {
    let basename = crate::lifecycle::RepoBasename::from_repo_root(repo_root)?;
    Some(crate::lifecycle::PlanId::new(basename, plan_key.clone()))
}
