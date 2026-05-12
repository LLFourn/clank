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
pub use watcher::{Watcher, WatcherEvent};

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

    let (watcher, watcher_rx) = Watcher::start()?;
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
    watcher: &Arc<Watcher>,
) -> anyhow::Result<()> {
    let all = sessions::list_active(pool).await?;
    for session in all {
        let sid = SessionId::from(session.id.clone());
        if let Err(err) = lifecycle.seed(&sid).await {
            tracing::warn!(session_id = sid.as_str(), error = ?err, "recovery: cache seed failed; session left in degraded state");
            continue;
        }

        let plan_path = PlanFilePath::from(session.plan_file_path.clone());
        if Path::new(&session.plan_file_path).exists() {
            watcher.switch_plan_file(&sid, &plan_path);
        }

        // Re-attach git logs/HEAD watcher and feedback dir watcher.
        // These are companions to the plan file; they exist for every
        // session, not just active-plan ones.
        let repo = Path::new(&session.repo_root);
        if repo.exists()
            && let Ok(logs_head) = git::resolve_git_logs_head(repo).await
            && logs_head.exists()
        {
            watcher.watch_git_logs(&sid, &logs_head);
        }
        let feedback_dir = repo.join(".trinity").join("feedback").join(sid.as_str());
        if feedback_dir.is_dir() {
            watcher.watch_feedback_dir(&sid, &feedback_dir);
        }

        let Some(active) = lifecycle.peek(&sid).await else {
            continue;
        };

        // Plan body drift.
        let body = match tokio::fs::read_to_string(&session.plan_file_path).await {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(session_id = sid.as_str(), path = %session.plan_file_path, "recovery: plan file missing for active session");
                None
            }
            Err(e) => return Err(e.into()),
        };
        if let Some(body) = body {
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
                let head = match git::rev_parse_head(repo).await {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::warn!(session_id = sid.as_str(), error = ?e, "recovery: git rev-parse HEAD failed; skipping plan drift");
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
                    tracing::warn!(session_id = sid.as_str(), error = ?err, "recovery: plan drift observation failed");
                }
            }
        }

        // HEAD drift. Drift-observe only when HEAD differs from the
        // relevant baseline per the planning/implementing rules. For
        // planning, that's `plans.base_commit` — HEAD moving past base
        // means a commit happened while daemon was off. For
        // implementing, it's `latest impl SHA` — HEAD differs means a
        // new commit (or a reset).
        if let Err(err) = recover_head_drift(lifecycle, &sid, &session).await {
            tracing::warn!(session_id = sid.as_str(), error = ?err, "recovery: HEAD drift check failed");
        }

        // Feedback dir drift. List the dir; queue ingests for new or
        // changed files, mark missing for sidecar rows whose file is gone.
        if feedback_dir.is_dir()
            && let Err(err) = recover_feedback_drift(lifecycle, &sid, &feedback_dir).await
        {
            tracing::warn!(session_id = sid.as_str(), error = ?err, "recovery: feedback drift check failed");
        }
    }

    Ok(())
}

async fn recover_head_drift(
    lifecycle: &Arc<SessionService>,
    sid: &SessionId,
    session: &crate::storage::sessions::Session,
) -> anyhow::Result<()> {
    let repo = Path::new(&session.repo_root);
    let head_sha = git::rev_parse_head(repo).await?;

    let Some(active_plan_id) = session.active_plan_id else {
        return Ok(());
    };
    let plan = sqlx::query_as::<_, crate::storage::plans::Plan>("SELECT * FROM plans WHERE id = ?")
        .bind(active_plan_id)
        .fetch_one(lifecycle.pool())
        .await?;

    let baseline = if plan.state == "implementing" {
        crate::storage::implementation_revisions::latest_for_plan(lifecycle.pool(), active_plan_id)
            .await?
            .map(|r| r.commit_sha)
    } else {
        Some(plan.base_commit.clone())
    };

    let differs = baseline.as_deref() != Some(&head_sha);
    if !differs {
        return Ok(());
    }

    // HEAD moved while we were off. Build a snapshot from current HEAD
    // and feed CommitObserved; the apply layer handles the
    // SHA-already-known case (reset) or INSERTs a new row.
    let parent = git::parent_sha(repo, &head_sha).await?;
    let branch = git::current_branch(repo).await?;
    let message = git::commit_message(repo, &head_sha).await?;
    let stat = git::diff_stat(repo, parent.as_deref(), &head_sha).await?;
    let porcelain = git::worktree_porcelain(repo).await.unwrap_or_default();
    let dirty = !porcelain.trim().is_empty();
    let commit = crate::lifecycle::CommitSnapshot {
        sha: CommitSha::from(head_sha),
        parent_sha: parent.map(CommitSha::from),
        branch,
        message,
        diff_stat: stat,
        worktree_status: Some(if dirty { porcelain } else { "clean".into() }),
        is_head: true,
    };
    lifecycle
        .observe(sid, "system:resume", Observation::CommitObserved { commit })
        .await?;
    Ok(())
}

async fn recover_feedback_drift(
    lifecycle: &Arc<SessionService>,
    sid: &SessionId,
    feedback_dir: &Path,
) -> anyhow::Result<()> {
    use crate::storage::feedback_files;

    let mut entries = tokio::fs::read_dir(feedback_dir).await?;
    let mut seen_labels: Vec<crate::lifecycle::AgentLabel> = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let extension_ok = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("md"))
            .unwrap_or(false);
        if !extension_ok {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !watcher::is_valid_slug(stem) {
            continue;
        }
        let label = crate::lifecycle::AgentLabel::from(stem);
        seen_labels.push(label.clone());

        // Queue an ingest pass via the same dispatch path the watcher
        // would have used. The dispatcher has its own no-op guard for
        // unchanged-body-AND-target.
        if let Err(err) = dispatch_feedback_changed(lifecycle, sid, &label, &path).await {
            tracing::warn!(session_id = sid.as_str(), label = label.as_str(), error = ?err, "recovery: feedback ingest failed");
        }
    }

    // For sidecar rows whose file is gone: mark missing.
    let rows = feedback_files::list_for_session(lifecycle.pool(), sid).await?;
    for row in rows {
        if seen_labels.iter().any(|l| l.as_str() == row.author_label) {
            continue;
        }
        let label = crate::lifecycle::AgentLabel::from(row.author_label.clone());
        if let Err(err) = dispatch_feedback_missing(lifecycle, sid, &label).await {
            tracing::warn!(session_id = sid.as_str(), label = label.as_str(), error = ?err, "recovery: mark-missing failed");
        }
    }

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
                WatcherEvent::HeadMoved { session_id } => {
                    if let Err(err) = dispatch_head_moved(&lifecycle, &session_id).await {
                        tracing::warn!(session_id = session_id.as_str(), error = ?err, "watcher head-moved dispatch failed");
                    }
                }
                WatcherEvent::FeedbackFileChanged {
                    session_id,
                    author_label,
                    path,
                } => {
                    if let Err(err) =
                        dispatch_feedback_changed(&lifecycle, &session_id, &author_label, &path)
                            .await
                    {
                        tracing::warn!(session_id = session_id.as_str(), error = ?err, "watcher feedback-changed dispatch failed");
                    }
                }
                WatcherEvent::FeedbackFileMissing {
                    session_id,
                    author_label,
                    path: _,
                } => {
                    if let Err(err) =
                        dispatch_feedback_missing(&lifecycle, &session_id, &author_label).await
                    {
                        tracing::warn!(session_id = session_id.as_str(), error = ?err, "watcher feedback-missing dispatch failed");
                    }
                }
            }
        }
    })
}

/// Build a CommitSnapshot for the repo's current HEAD and feed it
/// through the lifecycle. The reducer always emits `RecordImplementation`
/// for `CommitObserved`; the apply layer is the idempotency layer
/// (SHA-exists check + `head_reset_to_known_sha` audit event).
async fn dispatch_head_moved(
    lifecycle: &Arc<SessionService>,
    session_id: &SessionId,
) -> anyhow::Result<()> {
    let session = sessions::fetch(lifecycle.pool(), session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session vanished: {}", session_id.as_str()))?;

    // Drop if no active plan; the reducer would reject CommitObserved
    // against None and produce a noisy log entry.
    if session.active_plan_id.is_none() {
        tracing::debug!(
            session_id = session_id.as_str(),
            "watcher: HEAD moved but no active plan; dropping"
        );
        return Ok(());
    }

    let repo = Path::new(&session.repo_root);
    let head_sha = git::rev_parse_head(repo).await?;
    let parent = git::parent_sha(repo, &head_sha).await?;
    let branch = git::current_branch(repo).await?;
    let message = git::commit_message(repo, &head_sha).await?;
    let stat = git::diff_stat(repo, parent.as_deref(), &head_sha).await?;
    let porcelain = git::worktree_porcelain(repo).await.unwrap_or_default();
    let dirty = !porcelain.trim().is_empty();

    let commit = crate::lifecycle::CommitSnapshot {
        sha: CommitSha::from(head_sha),
        parent_sha: parent.map(CommitSha::from),
        branch,
        message,
        diff_stat: stat,
        worktree_status: Some(if dirty { porcelain } else { "clean".into() }),
        is_head: true,
    };

    lifecycle
        .observe(
            session_id,
            "system:git-watcher",
            Observation::CommitObserved { commit },
        )
        .await?;
    Ok(())
}

/// Ingest a feedback markdown file dropped under a watched feedback
/// directory. Computes the active review target (HEAD-derived for
/// implementing plans, so the ingest target matches what `get_context`
/// reports), then calls `SessionService::put_feedback` internally.
///
/// No-op guard is target-aware: if the file hash equals the previous
/// observation AND the target hasn't moved, the call returns early. A
/// target-only change re-ingests the same body against the new target.
async fn dispatch_feedback_changed(
    lifecycle: &Arc<SessionService>,
    session_id: &SessionId,
    author_label: &crate::lifecycle::AgentLabel,
    path: &Path,
) -> anyhow::Result<()> {
    use crate::storage::feedback_files;

    let session = sessions::fetch(lifecycle.pool(), session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session vanished: {}", session_id.as_str()))?;

    let body = match tokio::fs::read_to_string(path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Vanished between watcher debounce and read; treat as missing.
            return dispatch_feedback_missing(lifecycle, session_id, author_label).await;
        }
        Err(e) => return Err(e.into()),
    };
    let hash = blake3::hash(body.as_bytes()).to_hex().to_string();

    // Resolve the active target before the no-op guard so the guard can
    // include target equality.
    let active_target = resolve_active_feedback_target(lifecycle, &session).await?;

    let now = chrono::Utc::now().timestamp();
    let mut tx = lifecycle.pool().begin().await?;

    let existing = sqlx::query_as::<_, feedback_files::FeedbackFile>(
        "SELECT * FROM feedback_files WHERE session_id = ? AND author_label = ?",
    )
    .bind(session_id.as_str())
    .bind(author_label.as_str())
    .fetch_optional(&mut *tx)
    .await?;

    // Target-aware no-op guard. Skip when hash hasn't changed AND the
    // last successful ingest already targets the current active target.
    // Note: if there was a parse_error on the previous attempt we must
    // not no-op even if hash matches, because we'd be leaving the row
    // stuck in the error state when the underlying cause may have cleared.
    if let Some(row) = existing.as_ref() {
        if row.parse_error.is_none()
            && row.last_observed_hash.as_deref() == Some(hash.as_str())
            && target_matches(row, &active_target)
        {
            return Ok(());
        }
    }

    let path_str = path.to_string_lossy().into_owned();
    feedback_files::upsert_observed(&mut tx, session_id, author_label, &path_str, now).await?;
    feedback_files::set_observed(&mut tx, session_id, author_label, &hash, now).await?;

    let Some(target) = active_target else {
        feedback_files::set_parse_error(&mut tx, session_id, author_label, "no_active_plan", now)
            .await?;
        tx.commit().await?;
        return Ok(());
    };

    if body.trim().is_empty() {
        feedback_files::set_parse_error(&mut tx, session_id, author_label, "empty_body", now)
            .await?;
        tx.commit().await?;
        return Ok(());
    }

    // Commit the watcher observation before calling put_feedback;
    // put_feedback opens its own transaction and we don't want them
    // entangled. The sidecar's last_observed_* will be correct
    // regardless of whether put_feedback succeeds.
    tx.commit().await?;

    let (target_kind, target_id_str, target_ref) = target;

    match lifecycle
        .put_feedback(session_id, author_label, target_ref, body)
        .await
    {
        Ok(_) => {
            let mut tx = lifecycle.pool().begin().await?;
            feedback_files::set_ingested(
                &mut tx,
                session_id,
                author_label,
                &hash,
                target_kind,
                &target_id_str,
                now,
            )
            .await?;
            tx.commit().await?;
            Ok(())
        }
        Err(err) => {
            let mut tx = lifecycle.pool().begin().await?;
            feedback_files::set_parse_error(
                &mut tx,
                session_id,
                author_label,
                &err.to_string(),
                now,
            )
            .await?;
            tx.commit().await?;
            Ok(())
        }
    }
}

/// Compute the (kind, id, ref) tuple for the current active review
/// target. None when the session has no active plan.
///
/// For implementing plans we HEAD-derive: prefer the impl revision
/// matching git HEAD; fall back to the latest impl row. This mirrors
/// `get_context.active_target` resolution so the target the agent saw
/// is the target the ingest will record against.
async fn resolve_active_feedback_target(
    lifecycle: &Arc<SessionService>,
    session: &crate::storage::sessions::Session,
) -> anyhow::Result<
    Option<(
        crate::domain::TargetKind,
        String,
        crate::domain::FeedbackTargetRef,
    )>,
> {
    use crate::domain::{FeedbackTargetRef, TargetKind};
    use crate::storage::{implementation_revisions as impl_revs, plan_revisions};

    let Some(active_id) = session.active_plan_id else {
        return Ok(None);
    };
    let state: String = sqlx::query_scalar("SELECT state FROM plans WHERE id = ?")
        .bind(active_id)
        .fetch_one(lifecycle.pool())
        .await?;

    if state == "implementing" {
        let head_sha = git::rev_parse_head(Path::new(&session.repo_root)).await?;
        let head_sha_obj = CommitSha::from(head_sha.clone());
        let chosen =
            match impl_revs::fetch_by_sha(lifecycle.pool(), active_id, &head_sha_obj).await? {
                Some(rev) => Some(rev),
                None => impl_revs::latest_for_plan(lifecycle.pool(), active_id).await?,
            };
        Ok(chosen.map(|rev| {
            let sha = rev.commit_sha;
            (
                TargetKind::ImplementationCommit,
                sha.clone(),
                FeedbackTargetRef::ImplementationCommit(CommitSha::from(sha)),
            )
        }))
    } else if state == "planning" {
        let latest = plan_revisions::latest_for_plan(lifecycle.pool(), active_id).await?;
        Ok(latest.map(|rev| {
            (
                TargetKind::PlanRevision,
                rev.id.to_string(),
                FeedbackTargetRef::PlanRevision(rev.id),
            )
        }))
    } else {
        Ok(None)
    }
}

fn target_matches(
    row: &crate::storage::feedback_files::FeedbackFile,
    cur: &Option<(
        crate::domain::TargetKind,
        String,
        crate::domain::FeedbackTargetRef,
    )>,
) -> bool {
    match (
        cur,
        &row.last_ingested_target_kind,
        &row.last_ingested_target_id,
    ) {
        (Some((kind, id, _)), Some(rk), Some(rid)) => rk == kind.as_str() && rid == id,
        _ => false,
    }
}

/// Feedback file vanished from disk. Clear the sidecar's
/// `last_observed_hash` (derived status flips to `missing`).
/// Historical `feedback` rows are preserved — retraction is v2.
async fn dispatch_feedback_missing(
    lifecycle: &Arc<SessionService>,
    session_id: &SessionId,
    author_label: &crate::lifecycle::AgentLabel,
) -> anyhow::Result<()> {
    use crate::storage::feedback_files;
    let now = chrono::Utc::now().timestamp();
    let mut tx = lifecycle.pool().begin().await?;
    feedback_files::mark_missing(&mut tx, session_id, author_label, now).await?;
    tx.commit().await?;
    Ok(())
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
