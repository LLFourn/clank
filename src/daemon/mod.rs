//! Daemon entry point. Wires together: storage pool, file watcher, lifecycle
//! service, HTTP server. Owns the startup recovery sequence per the plan's
//! "Restart / recovery model" section.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Args;
use sqlx::SqlitePool;
use tokio::sync::mpsc;

pub(crate) mod apply;
pub(crate) mod curator;
pub(crate) mod git;
pub mod http;
pub(crate) mod internal_api;
pub(crate) mod service;
mod ui;
pub(crate) mod watcher;

pub use service::{
    CurrentFeedbackView, FeedbackUpsertOutcome, LifecycleServiceError, ObservationOutcome,
    ServiceError, SessionService,
};
pub use watcher::{PlanWatcher, WatcherEvent};

use crate::lifecycle::{CommitSha, Observation, PlanFilePath, SessionId, content_hash};
use crate::storage::{plan_revisions, plans, sessions};

#[derive(Args, Debug, Clone)]
pub struct ServeArgs {
    /// Path to the SQLite database file.
    #[arg(long, default_value = "~/.trinity/trinity.sqlite", env = "TRINITY_DB")]
    pub db: String,

    /// Address to bind the HTTP server to. Defaults to 127.0.0.1:7777
    /// (loopback). Do not bind to non-loopback addresses without adding
    /// auth.
    #[arg(long, default_value = "127.0.0.1:7777", env = "TRINITY_BIND")]
    pub bind: SocketAddr,
}

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub lifecycle: Arc<SessionService>,
}

pub async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let db_path = expand_home(&args.db);
    let (state, _shutdown) = build_state(&db_path).await?;
    let app = http::router(state);

    tracing::info!(bind = %args.bind, "trinity listening");
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// A handle that cleanly stops every background task the daemon started
/// (watcher dispatcher, etc.). Tests use this to truly tear down a daemon
/// before restarting from the same DB; in production it's just dropped at
/// process exit.
pub struct DaemonShutdown {
    dispatcher: tokio::task::JoinHandle<()>,
}

impl DaemonShutdown {
    pub async fn stop(self) {
        self.dispatcher.abort();
        let _ = self.dispatcher.await;
    }
}

/// Open the pool, run migrations, spin up the watcher, build the lifecycle
/// service, perform startup recovery + drift detection. Returns the
/// `AppState` plus a shutdown handle. Used by `serve` and by integration
/// tests.
pub async fn build_state(db_path: &Path) -> anyhow::Result<(AppState, DaemonShutdown)> {
    tracing::info!(db = %db_path.display(), "opening database");
    let pool = crate::storage::open_pool(db_path).await?;

    let (watcher, watcher_rx) = PlanWatcher::start()?;
    let lifecycle = Arc::new(SessionService::new(pool.clone(), Arc::clone(&watcher)));

    // Recovery: rebuild per-session cache from SQL, then run drift detection
    // only for sessions with an active plan.
    recover(&pool, &lifecycle, &watcher).await?;

    let dispatcher = spawn_watcher_dispatch(Arc::clone(&lifecycle), watcher_rx);

    Ok((AppState { pool, lifecycle }, DaemonShutdown { dispatcher }))
}

/// Phase 2 + 3 of the startup sequence from the plan:
/// - Seed per-session cache from SQL (no observations fed).
/// - Re-establish watchers for every active or inactive session whose
///   plan file is on disk.
/// - Phase 4: drift detection — only for sessions with an active plan.
async fn recover(
    pool: &SqlitePool,
    lifecycle: &Arc<SessionService>,
    watcher: &Arc<PlanWatcher>,
) -> anyhow::Result<()> {
    let all = sessions::list_active(pool).await?;
    for session in all {
        let sid = SessionId::from(session.id.clone());
        // Phase 2: cache load. Errors are logged but don't crash the daemon.
        if let Err(err) = lifecycle.seed(&sid).await {
            tracing::warn!(session_id = sid.as_str(), error = ?err, "recovery: cache seed failed; session left in degraded state");
            continue;
        }

        // Phase 3: re-establish the watcher on the recorded plan-file path
        // if the file exists on disk. Sessions without an active plan still
        // get watched, but the dispatcher drops their events at phase 5.
        let plan_path = PlanFilePath::from(session.plan_file_path.clone());
        if Path::new(&session.plan_file_path).exists() {
            watcher.switch(&sid, &plan_path);
        }

        // Phase 4: drift detection — only for sessions whose cache is Some.
        let Some(active) = lifecycle.peek(&sid).await else {
            continue;
        };
        let body = match tokio::fs::read_to_string(&session.plan_file_path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(session_id = sid.as_str(), path = %session.plan_file_path, "recovery: plan file missing for active session");
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let cur_hash = content_hash(&body);
        let on_record = match &active {
            crate::lifecycle::ActivePlan::Planning {
                latest_plan_hash, ..
            }
            | crate::lifecycle::ActivePlan::Implementing {
                latest_plan_hash, ..
            } => latest_plan_hash.clone(),
        };
        if cur_hash != on_record {
            // File drifted while daemon was off. Feed PlanFileObserved.
            // The reducer decides: planning → revision; implementing → archive+start.
            let head = match git::rev_parse_head(Path::new(&session.repo_root)).await {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(session_id = sid.as_str(), error = ?e, "recovery: git rev-parse HEAD failed; skipping drift");
                    continue;
                }
            };
            if let Err(err) = lifecycle
                .observe(
                    &sid,
                    "system:resume",
                    Observation::PlanFileObserved {
                        body,
                        head: CommitSha::from(head),
                    },
                )
                .await
            {
                tracing::warn!(session_id = sid.as_str(), error = ?err, "recovery: drift observation failed");
            }
        }
    }

    // Reset all agents' last_seen so masters/reviewers must re-bind.
    crate::storage::agents::reset_all_to_stale(pool, 0).await?;
    Ok(())
}

fn spawn_watcher_dispatch(
    lifecycle: Arc<SessionService>,
    mut watcher_rx: mpsc::UnboundedReceiver<WatcherEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = watcher_rx.recv().await {
            match event {
                WatcherEvent::PlanFileDirty { session_id } => {
                    if let Err(err) = dispatch_dirty(&lifecycle, &session_id).await {
                        tracing::warn!(session_id = session_id.as_str(), error = ?err, "watcher dispatch failed");
                    }
                }
                WatcherEvent::PlanFileMissing { session_id, path } => {
                    if let Err(err) = record_plan_file_missing(&lifecycle, &session_id, &path).await
                    {
                        tracing::warn!(session_id = session_id.as_str(), error = ?err, "watcher missing-event failed");
                    }
                }
            }
        }
    })
}

async fn dispatch_dirty(
    lifecycle: &Arc<SessionService>,
    session_id: &SessionId,
) -> anyhow::Result<()> {
    let session = sessions::fetch(lifecycle.pool(), session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session vanished: {}", session_id.as_str()))?;

    // Phase 5: drop the event if there's no active plan. The reducer would
    // reject `PlanFileObserved` against `None` anyway, but we'd rather not
    // even attempt and produce a noisy log entry.
    if session.active_plan_id.is_none() {
        tracing::debug!(
            session_id = session_id.as_str(),
            "watcher: plan-file dirty but no active plan; dropping (use register_plan_file to start new lifecycle)"
        );
        return Ok(());
    }

    let body = match tokio::fs::read_to_string(&session.plan_file_path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return record_plan_file_missing(
                lifecycle,
                session_id,
                Path::new(&session.plan_file_path),
            )
            .await;
        }
        Err(e) => return Err(e.into()),
    };
    let head = git::rev_parse_head(Path::new(&session.repo_root)).await?;
    lifecycle
        .observe(
            session_id,
            "system:watcher",
            Observation::PlanFileObserved {
                body,
                head: CommitSha::from(head),
            },
        )
        .await?;
    Ok(())
}

async fn record_plan_file_missing(
    lifecycle: &Arc<SessionService>,
    session_id: &SessionId,
    path: &Path,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp();
    crate::storage::events::append(
        lifecycle.pool(),
        session_id,
        None,
        None,
        None,
        crate::domain::EventKind::PlanFileMissing.as_str(),
        "system:watcher",
        &serde_json::json!({"path": path.display().to_string()}),
        None,
        now,
    )
    .await?;
    tracing::warn!(session_id = session_id.as_str(), path = %path.display(), "watcher: plan file missing");
    Ok(())
}

// Suppress dead-code warnings on items only used by other modules during
// development of the rewrite.
#[allow(dead_code)]
fn _ensure_used(_: &plans::Plan, _: &plan_revisions::PlanRevision) {}

fn expand_home(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(s)
}
