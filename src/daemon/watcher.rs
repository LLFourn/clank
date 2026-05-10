use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use sqlx::SqlitePool;
use tokio::sync::mpsc;

use crate::domain::{EventKind, TargetKind};
use crate::storage::{events as ev_store, plan_revisions};

/// Manages the file-watcher debouncer and the path→plan_id mapping.
pub struct PlanWatcher {
    inner: Arc<Mutex<WatcherInner>>,
}

struct WatcherInner {
    debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
    /// Canonical plan-file path → plan_id.
    paths: HashMap<PathBuf, String>,
    /// Canonical parent directory → set of plan paths under it (for cleanup).
    parents: HashMap<PathBuf, usize>,
}

const DEBOUNCE_TIMEOUT: Duration = Duration::from_millis(1500);

impl PlanWatcher {
    pub fn start(pool: SqlitePool) -> anyhow::Result<Arc<Self>> {
        let (tx, mut rx) = mpsc::unbounded_channel::<DebounceEventResult>();

        let debouncer = new_debouncer(DEBOUNCE_TIMEOUT, None, move |res| {
            let _ = tx.send(res);
        })?;

        let inner = Arc::new(Mutex::new(WatcherInner {
            debouncer,
            paths: HashMap::new(),
            parents: HashMap::new(),
        }));

        let watcher = Arc::new(PlanWatcher { inner });
        let watcher_for_task = Arc::clone(&watcher);
        tokio::spawn(async move {
            while let Some(result) = rx.recv().await {
                match result {
                    Ok(events) => {
                        // Collect distinct paths that match a registered plan file.
                        let mut touched: Vec<PathBuf> = Vec::new();
                        {
                            let inner = watcher_for_task.inner.lock().unwrap();
                            for ev in events {
                                for path in ev.paths.iter() {
                                    if inner.paths.contains_key(path) && !touched.contains(path) {
                                        touched.push(path.clone());
                                    }
                                }
                            }
                        }
                        for path in touched {
                            if let Err(err) =
                                check_and_snapshot(&pool, &watcher_for_task, &path).await
                            {
                                tracing::warn!(path = %path.display(), error = ?err, "failed to snapshot plan file");
                            }
                        }
                    }
                    Err(errs) => {
                        for err in errs {
                            tracing::warn!(error = ?err, "file watcher error");
                        }
                    }
                }
            }
        });

        Ok(watcher)
    }

    /// Watch a plan file. Idempotent.
    ///
    /// `plan_path` must be an already-canonicalized absolute path.
    pub fn watch(&self, plan_path: &Path, plan_id: &str) -> anyhow::Result<()> {
        let parent = match plan_path.parent() {
            Some(p) => p.to_path_buf(),
            None => anyhow::bail!("plan_path has no parent: {}", plan_path.display()),
        };
        let mut inner = self.inner.lock().unwrap();
        if inner.paths.contains_key(plan_path) {
            return Ok(());
        }
        let prior_count = inner.parents.get(&parent).copied().unwrap_or(0);
        if prior_count == 0 {
            inner
                .debouncer
                .watch(&parent, RecursiveMode::NonRecursive)?;
        }
        inner.parents.insert(parent, prior_count + 1);
        inner
            .paths
            .insert(plan_path.to_path_buf(), plan_id.to_string());
        tracing::info!(path = %plan_path.display(), plan_id = plan_id, "watching plan file");
        Ok(())
    }

    /// Stop watching a plan file (e.g., on archive). Idempotent.
    pub fn unwatch(&self, plan_path: &Path) -> anyhow::Result<()> {
        let parent = match plan_path.parent() {
            Some(p) => p.to_path_buf(),
            None => return Ok(()),
        };
        let mut inner = self.inner.lock().unwrap();
        if inner.paths.remove(plan_path).is_none() {
            return Ok(());
        }
        if let Some(count) = inner.parents.get_mut(&parent) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                inner.parents.remove(&parent);
                let _ = inner.debouncer.unwatch(&parent);
            }
        }
        Ok(())
    }

    fn lookup_plan_id(&self, plan_path: &Path) -> Option<String> {
        self.inner.lock().unwrap().paths.get(plan_path).cloned()
    }
}

async fn check_and_snapshot(
    pool: &SqlitePool,
    watcher: &PlanWatcher,
    plan_path: &Path,
) -> anyhow::Result<()> {
    let plan_id = match watcher.lookup_plan_id(plan_path) {
        Some(id) => id,
        None => return Ok(()),
    };
    let now = chrono::Utc::now().timestamp();

    match tokio::fs::read_to_string(plan_path).await {
        Ok(body) => {
            let hash = plan_revisions::compute_content_hash(&body);
            let latest = plan_revisions::latest(pool, &plan_id).await?;
            if latest.as_ref().map(|r| r.content_hash.as_str()) == Some(hash.as_str()) {
                return Ok(());
            }

            // v0 lifecycle gate: plan-file edits create plan_revisions only
            // while the plan is in `planning` or `plan_approved`. After
            // `implementation_review`, `done`, or `archived`, edits become
            // "needs human lifecycle action" warnings instead of revisions.
            // Gating on state (not just `current_implementation_id`) catches
            // the case of a plan that went `plan_approved` → `done` without
            // ever registering an impl commit.
            let plan = crate::storage::plans::fetch(pool, &plan_id).await?;
            let accepts = plan
                .as_ref()
                .and_then(|p| crate::domain::WorkState::parse(&p.state))
                .map(|s| s.accepts_plan_revisions())
                .unwrap_or(false);
            if !accepts {
                let payload = serde_json::json!({
                    "path": plan_path.display().to_string(),
                    "new_content_hash": hash,
                    "note": "v0 ignores this edit; archive/rename the session to start a new cycle.",
                });
                ev_store::append(
                    pool,
                    &ev_store::NewEvent::note(
                        &plan_id,
                        EventKind::PlanFileChangedAfterImplementation,
                        "system:watcher",
                        &payload,
                        now,
                    ),
                )
                .await?;
                tracing::warn!(
                    plan_id = %plan_id,
                    path = %plan_path.display(),
                    "plan file edited after implementation commit registered; emitted warning, no new revision"
                );
                return Ok(());
            }

            let mut tx = pool.begin().await?;
            let revision_id =
                plan_revisions::append(&mut *tx, &plan_id, &hash, &body, now, "watcher").await?;
            let revision_id_str = revision_id.to_string();
            let payload = serde_json::json!({"detected_by": "watcher"});
            ev_store::append(
                &mut *tx,
                &ev_store::NewEvent::against_target(
                    &plan_id,
                    EventKind::PlanRevisionCreated,
                    "system:watcher",
                    &payload,
                    now,
                    ev_store::EventTarget {
                        kind: TargetKind::PlanRevision,
                        id: &revision_id_str,
                    },
                ),
            )
            .await?;
            crate::storage::plans::touch_updated_at(&mut *tx, &plan_id, now).await?;
            tx.commit().await?;
            tracing::info!(plan_id = %plan_id, revision_id, "appended plan revision via watcher");
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // File missing — emit event but don't auto-archive.
            let payload = serde_json::json!({"path": plan_path.display().to_string()});
            ev_store::append(
                pool,
                &ev_store::NewEvent::note(
                    &plan_id,
                    EventKind::PlanFileMissing,
                    "system:watcher",
                    &payload,
                    now,
                ),
            )
            .await?;
            tracing::warn!(plan_id = %plan_id, path = %plan_path.display(), "plan file missing");
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
