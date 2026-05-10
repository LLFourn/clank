use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;
use sqlx::SqlitePool;

pub(crate) mod curator;
pub(crate) mod git;
pub mod http;
pub(crate) mod internal_api;
mod ui;
pub(crate) mod watcher;

pub use watcher::PlanWatcher;

#[derive(Args, Debug, Clone)]
pub struct ServeArgs {
    /// Path to the SQLite database file.
    #[arg(long, default_value = "~/.trinity/trinity.sqlite", env = "TRINITY_DB")]
    pub db: String,

    /// Address to bind the HTTP server to. Defaults to 127.0.0.1:7777 (loopback).
    /// Do not bind to non-loopback addresses without adding auth.
    #[arg(long, default_value = "127.0.0.1:7777", env = "TRINITY_BIND")]
    pub bind: SocketAddr,
}

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub watcher: Arc<PlanWatcher>,
}

pub async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let db_path = expand_home(&args.db);
    let state = build_state(&db_path).await?;
    let app = http::router(state);

    tracing::info!(bind = %args.bind, "trinity listening");
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Open the database, start the file watcher, and re-establish state for any
/// non-archived plans. Used by `serve` and by integration tests.
pub async fn build_state(db_path: &std::path::Path) -> anyhow::Result<AppState> {
    tracing::info!(db = %db_path.display(), "opening database");
    let pool = crate::storage::open_pool(db_path).await?;
    let watcher = PlanWatcher::start(pool.clone())?;
    restore_state(&pool, &watcher).await?;
    Ok(AppState { pool, watcher })
}

/// On daemon startup: re-establish watchers for all non-archived plans, snapshot
/// any whose file content hash drifted while the daemon was down, and reset
/// every agent's `last_seen` so masters/reviewers must re-bind.
async fn restore_state(pool: &SqlitePool, watcher: &PlanWatcher) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp();
    crate::storage::agents::reset_all_to_stale(pool, 0).await?;

    for plan in crate::storage::plans::list_active(pool).await? {
        let Some(plan_path) = plan.plan_path() else {
            continue; // submit_plan fallback (no file) — nothing to watch.
        };
        if let Err(err) = watcher.watch(&plan_path, &plan.id) {
            tracing::warn!(plan_id = %plan.id, path = %plan_path.display(), error = ?err, "failed to restore watcher");
            continue;
        }
        // Resume snapshot if the file drifted while we were off — but only
        // before implementation has been registered. After impl, surface as a
        // warning event (same v0 lifecycle gate as the live watcher).
        match tokio::fs::read_to_string(&plan_path).await {
            Ok(body) => {
                let hash = crate::storage::plan_revisions::compute_content_hash(&body);
                let latest = crate::storage::plan_revisions::latest(pool, &plan.id).await?;
                if latest.as_ref().map(|r| r.content_hash.as_str()) != Some(hash.as_str()) {
                    let accepts_revisions = crate::domain::WorkState::parse(&plan.state)
                        .map(|s| s.accepts_plan_revisions())
                        .unwrap_or(false);
                    if !accepts_revisions {
                        let payload = serde_json::json!({
                            "path": plan_path.display().to_string(),
                            "new_content_hash": hash,
                            "note": "drift detected on daemon restart after implementation; v0 ignores.",
                        });
                        crate::storage::events::append(
                            pool,
                            &crate::storage::events::NewEvent::note(
                                &plan.id,
                                crate::domain::EventKind::PlanFileChangedAfterImplementation,
                                "system:resume",
                                &payload,
                                now,
                            ),
                        )
                        .await?;
                        tracing::warn!(plan_id = %plan.id, "post-impl plan-file drift on resume; warning event only");
                        continue;
                    }
                    let mut tx = pool.begin().await?;
                    let revision_id = crate::storage::plan_revisions::append(
                        &mut *tx,
                        &plan.id,
                        &hash,
                        &body,
                        now,
                        "resume_snapshot",
                    )
                    .await?;
                    let revision_id_str = revision_id.to_string();
                    let payload = serde_json::json!({"detected_by": "resume_snapshot"});
                    crate::storage::events::append(
                        &mut *tx,
                        &crate::storage::events::NewEvent::against_target(
                            &plan.id,
                            crate::domain::EventKind::PlanRevisionCreated,
                            "system:resume",
                            &payload,
                            now,
                            crate::storage::events::EventTarget {
                                kind: crate::domain::TargetKind::PlanRevision,
                                id: &revision_id_str,
                            },
                        ),
                    )
                    .await?;
                    crate::storage::plans::touch_updated_at(&mut *tx, &plan.id, now).await?;
                    tx.commit().await?;
                    tracing::info!(plan_id = %plan.id, revision_id, "resume_snapshot inserted");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let payload = serde_json::json!({"path": plan_path.display().to_string()});
                crate::storage::events::append(
                    pool,
                    &crate::storage::events::NewEvent::note(
                        &plan.id,
                        crate::domain::EventKind::PlanFileMissing,
                        "system:resume",
                        &payload,
                        now,
                    ),
                )
                .await?;
                tracing::warn!(plan_id = %plan.id, path = %plan_path.display(), "plan file missing on resume");
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn expand_home(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(s)
}
