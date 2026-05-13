//! Daemon entry point. Wires together: storage pool, file watcher, lifecycle
//! service, HTTP server. Owns the startup recovery sequence per the plan's
//! "Restart / recovery model" section.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Args;
use sqlx::SqlitePool;
use tokio::sync::mpsc;

pub(crate) mod amend;
pub(crate) mod apply;
pub(crate) mod curator;
pub(crate) mod diff_parser;
pub(crate) mod feedback_status;
pub(crate) mod git;
pub mod http;
pub(crate) mod internal_api;
pub(crate) mod service;
mod ui;
mod ui_styles;
pub(crate) mod watcher;

pub use service::{
    ActiveTarget, CurrentFeedbackView, FeedbackUpsertOutcome, LifecycleServiceError,
    ObservationOutcome, ServiceError, SessionService,
};
pub use watcher::{Watcher, WatcherEvent};

use crate::lifecycle::{CommitSha, Observation, PlanFilePath, SessionId, content_hash};
use crate::storage::sessions;

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
    let (state, _shutdown) = build_state(&db_path, DaemonConfig::default()).await?;
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

/// Runtime knobs threaded through `build_state`. Production callers use
/// `DaemonConfig::default()`; integration tests construct one with a
/// short `watcher_debounce` so the suite isn't held hostage to the
/// real-world debounce window.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// notify-debouncer-full coalescing window. Production default is
    /// long enough to absorb editor save flurries and bulk
    /// filesystem operations (`git checkout`, IDE format-on-save).
    pub watcher_debounce: std::time::Duration,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            watcher_debounce: std::time::Duration::from_millis(1500),
        }
    }
}

/// Open the pool, run migrations, spin up the watcher, build the lifecycle
/// service, perform startup recovery + drift detection. Returns the
/// `AppState` plus a shutdown handle. Used by `serve` and by integration
/// tests.
pub async fn build_state(
    db_path: &Path,
    config: DaemonConfig,
) -> anyhow::Result<(AppState, DaemonShutdown)> {
    tracing::info!(db = %db_path.display(), "opening database");
    let pool = crate::storage::open_pool(db_path).await?;

    let (watcher, watcher_rx) = Watcher::start(config.watcher_debounce)?;
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
    for sid in lifecycle
        .archive_superseded_active_sessions("system:recover")
        .await?
    {
        tracing::info!(
            session_id = sid.as_str(),
            "recovery: archived superseded active session"
        );
    }

    // Discover known repos and attach plan-dir watchers; also scan each
    // for `.md` files that have no session yet and auto-register them.
    let known_repos: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT repo_root FROM sessions",
    )
    .fetch_all(pool)
    .await?;
    for repo_str in known_repos {
        let repo_root = Path::new(&repo_str);
        let plans_dir = repo_root.join(".trinity").join("plans");
        if !plans_dir.is_dir() {
            continue;
        }
        if let Err(err) = watcher.watch_plan_dir(repo_root, &plans_dir) {
            tracing::warn!(repo_root = %repo_root.display(), error = ?err, "recovery: plan-dir watcher attach failed");
        }
        // Scan existing files. Auto-registration is idempotent against
        // already-known sessions and handles reactivation.
        let mut read_dir = match tokio::fs::read_dir(&plans_dir).await {
            Ok(d) => d,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let path = entry.path();
            let is_md = path
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("md"))
                .unwrap_or(false);
            if !is_md {
                continue;
            }
            // Recovery is strict about reactivation: a session that
            // already exists *in this repo* (active or archived) is
            // left alone. An archived session means delete; the plan
            // file stays on disk by design, and a restart-driven scan
            // must NOT undo that delete. An active session is already
            // wired and the per-file watcher handles edits.
            //
            // But — when the bare basename collides with a session in
            // a *different* repo, recovery must do the same
            // disambiguation the live `PlanDirFileCreated` path does
            // (basename + 6-char repo hash) and check the disambiguated
            // id against this repo's known sessions. Skipping on bare
            // collision alone strands cross-repo collisions until a
            // live notify event happens to fire.
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let resolved =
                match resolve_auto_register_session_id(lifecycle, stem, &repo_str).await {
                    Ok(Some(id)) => id,
                    _ => continue,
                };
            let same_repo_match = sessions::fetch(pool, &resolved)
                .await?
                .map(|s| s.repo_root == repo_str)
                .unwrap_or(false);
            if same_repo_match {
                continue;
            }
            if let Err(err) =
                dispatch_plan_dir_file_created(lifecycle, repo_root, &path).await
            {
                tracing::warn!(path = %path.display(), error = ?err, "recovery: auto-register scan failed");
            }
        }
    }

    let all = sessions::list_active(pool).await?;
    for session in all {
        let sid = SessionId::from(session.id.clone());
        if let Err(err) = lifecycle.seed(&sid).await {
            tracing::warn!(session_id = sid.as_str(), error = ?err, "recovery: cache seed failed; session left in degraded state");
            continue;
        }

        let plan_path = PlanFilePath::from(session.plan_file_path.clone());
        if Path::new(&session.plan_file_path).exists()
            && let Err(err) = watcher.switch_plan_file(&sid, &plan_path)
        {
            tracing::warn!(session_id = sid.as_str(), error = ?err, "recovery: plan-file watcher attach failed");
        }

        // Re-attach git logs/HEAD watcher and feedback dir watcher.
        // These are companions to the plan file; they exist for every
        // session, not just active-plan ones.
        let repo = Path::new(&session.repo_root);
        if repo.exists()
            && let Ok(logs_head) = git::resolve_git_logs_head(repo).await
            && logs_head.exists()
            && let Err(err) = watcher.watch_git_logs(&sid, &logs_head)
        {
            tracing::warn!(session_id = sid.as_str(), error = ?err, "recovery: git logs watcher attach failed");
        }
        for kind in [
            crate::domain::FeedbackKind::Plan,
            crate::domain::FeedbackKind::Impl,
        ] {
            let feedback_dir = crate::feedback_path::feedback_dir(repo, &sid, kind);
            if feedback_dir.is_dir()
                && let Err(err) = watcher.watch_feedback_dir(&sid, kind, &feedback_dir)
            {
                tracing::warn!(session_id = sid.as_str(), kind = kind.as_str(), error = ?err, "recovery: feedback dir watcher attach failed");
            }
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

        // Feedback dir drift, per kind. List each dir; queue ingests
        // for new or changed files, mark missing for sidecar rows
        // whose file is gone.
        for kind in [
            crate::domain::FeedbackKind::Plan,
            crate::domain::FeedbackKind::Impl,
        ] {
            let dir = crate::feedback_path::feedback_dir(repo, &sid, kind);
            if dir.is_dir()
                && let Err(err) = recover_feedback_drift(lifecycle, &sid, kind, &dir).await
            {
                tracing::warn!(session_id = sid.as_str(), kind = kind.as_str(), error = ?err, "recovery: feedback drift check failed");
            }
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
    feedback_kind: crate::domain::FeedbackKind,
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
        if !crate::feedback_path::is_valid_slug(stem) {
            continue;
        }
        let label = crate::lifecycle::AgentLabel::from(stem);
        seen_labels.push(label.clone());

        if let Err(err) =
            dispatch_feedback_changed(lifecycle, sid, feedback_kind, &label, &path).await
        {
            tracing::warn!(session_id = sid.as_str(), kind = feedback_kind.as_str(), label = label.as_str(), error = ?err, "recovery: feedback ingest failed");
        }
    }

    // For sidecar rows of THIS kind whose file is gone: mark missing.
    let rows = feedback_files::list_for_session(lifecycle.pool(), sid).await?;
    for row in rows {
        if row.feedback_kind != feedback_kind.as_str() {
            continue;
        }
        if seen_labels.iter().any(|l| l.as_str() == row.author_label) {
            continue;
        }
        let label = crate::lifecycle::AgentLabel::from(row.author_label.clone());
        if let Err(err) = dispatch_feedback_missing(lifecycle, sid, feedback_kind, &label).await {
            tracing::warn!(session_id = sid.as_str(), kind = feedback_kind.as_str(), label = label.as_str(), error = ?err, "recovery: mark-missing failed");
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
                    feedback_kind,
                    author_label,
                    path,
                } => {
                    if let Err(err) = dispatch_feedback_changed(
                        &lifecycle,
                        &session_id,
                        feedback_kind,
                        &author_label,
                        &path,
                    )
                    .await
                    {
                        tracing::warn!(session_id = session_id.as_str(), error = ?err, "watcher feedback-changed dispatch failed");
                    }
                }
                WatcherEvent::FeedbackFileMissing {
                    session_id,
                    feedback_kind,
                    author_label,
                    path: _,
                } => {
                    if let Err(err) = dispatch_feedback_missing(
                        &lifecycle,
                        &session_id,
                        feedback_kind,
                        &author_label,
                    )
                    .await
                    {
                        tracing::warn!(session_id = session_id.as_str(), error = ?err, "watcher feedback-missing dispatch failed");
                    }
                }
                WatcherEvent::PlanDirFileCreated { repo_root, path } => {
                    if let Err(err) =
                        dispatch_plan_dir_file_created(&lifecycle, &repo_root, &path).await
                    {
                        tracing::warn!(path = %path.display(), error = ?err, "watcher plan-dir dispatch failed");
                    }
                }
            }
        }
    })
}

/// Auto-register a freshly-dropped `.md` file in `<repo>/.trinity/plans/`
/// as a session. The basename (minus `.md`) becomes the `session_id`.
/// Idempotent against existing sessions; archived sessions are reactivated.
async fn dispatch_plan_dir_file_created(
    lifecycle: &Arc<SessionService>,
    repo_root: &Path,
    path: &Path,
) -> anyhow::Result<()> {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return Ok(());
    };
    let body = match tokio::fs::read_to_string(path).await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(path = %path.display(), error = ?err, "auto-register: read failed");
            return Ok(());
        }
    };
    let plan_path = PlanFilePath::from(path.to_string_lossy().into_owned());
    let head_str = match super::daemon::git::rev_parse_head(repo_root).await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(repo_root = %repo_root.display(), error = ?err, "auto-register: rev-parse HEAD failed");
            return Ok(());
        }
    };
    let head = crate::lifecycle::CommitSha::from(head_str);

    let repo_root_str = repo_root.to_string_lossy().into_owned();
    let actor = "system:auto-register";

    // Pick a session_id, disambiguating with a repo-derived suffix if the
    // bare basename is already used by a different repo (plan §9).
    let session_id =
        match resolve_auto_register_session_id(lifecycle, stem, &repo_root_str).await? {
            Some(id) => id,
            None => {
                tracing::warn!(
                    stem,
                    repo_root = %repo_root_str,
                    "auto-register: could not pick a unique session_id; skipping"
                );
                return Ok(());
            }
        };

    let existing = sessions::fetch(lifecycle.pool(), &session_id).await?;

    if let Some(session) = existing.as_ref() {
        if session.archived_at.is_some() {
            // The watcher only emits `PlanDirFileCreated` for genuinely
            // new paths (it tracks a seen set per plan dir). An edit
            // to an already-on-disk plan file does NOT reach here.
            // Reactivation on a true fresh drop (remove+re-create or a
            // never-before-seen path) is the auto-register contract.
            lifecycle
                .reactivate_session(
                    &session_id,
                    &repo_root_str,
                    &plan_path,
                    body,
                    head,
                    actor,
                )
                .await?;
            // Maintain "at most one active session per repo" invariant.
            lifecycle
                .archive_other_active_sessions_in_repo(&repo_root_str, &session_id, actor)
                .await?;
            attach_session_watchers(lifecycle, &session_id, repo_root).await?;
            scan_feedback_dir_for_session(lifecycle, repo_root, &session_id).await;
            return Ok(());
        }
        // Active session already registered; per-file watcher handles edits.
        return Ok(());
    }

    // Fresh session: create the row and register the plan.
    let now = chrono::Utc::now().timestamp();
    let display_title = session_id.as_str().to_string();
    sessions::insert(
        lifecycle.pool(),
        &session_id,
        &repo_root_str,
        &plan_path,
        Some(&display_title),
        now,
    )
    .await?;
    lifecycle
        .archive_other_active_sessions_in_repo(&repo_root_str, &session_id, actor)
        .await?;
    lifecycle
        .observe(
            &session_id,
            actor,
            crate::lifecycle::Observation::PlanRegistered {
                path: plan_path,
                body,
                head,
            },
        )
        .await?;
    // `observe(PlanRegistered)` attached the per-file plan watcher.
    // Attach git-logs/HEAD, both feedback dirs, and the plan-dir
    // watcher (shared with reactivation + explicit register paths).
    attach_session_watchers(lifecycle, &session_id, repo_root).await?;
    Ok(())
}

/// Pick the session id for an auto-registered plan file. Returns the
/// bare basename when no conflicting session exists, or basename plus a
/// short repo-derived suffix when the bare id is already used by a
/// session in a *different* repo. Returns `Ok(None)` if every candidate
/// is still occupied by a foreign repo (extremely unlikely; logged so
/// we can investigate if it ever happens).
async fn resolve_auto_register_session_id(
    lifecycle: &Arc<SessionService>,
    stem: &str,
    repo_root_str: &str,
) -> anyhow::Result<Option<SessionId>> {
    // The session_id must satisfy the same slug rules MCP `register_plan_file`
    // enforces — otherwise `get_context` later rejects the id as
    // invalid. If the bare basename isn't slug-clean, reject (logged
    // by the caller); we don't try to munge it because the result
    // would no longer match what the file is named.
    let bare = match sessions::validate_slug(stem) {
        Ok(id) => id,
        Err(_) => return Ok(None),
    };
    if let Some(existing) = sessions::fetch(lifecycle.pool(), &bare).await? {
        if existing.repo_root == repo_root_str {
            return Ok(Some(bare));
        }
        // Bare id collides with a different repo. Try a short repo-hash
        // suffix; only fall back further if THAT also collides.
        let hash_hex = blake3::hash(repo_root_str.as_bytes()).to_hex();
        let suffix: String = hash_hex.chars().take(6).collect();
        let candidate = format!("{stem}-{suffix}");
        let disambiguated = match sessions::validate_slug(&candidate) {
            Ok(id) => id,
            Err(_) => return Ok(None),
        };
        if let Some(other) = sessions::fetch(lifecycle.pool(), &disambiguated).await? {
            if other.repo_root == repo_root_str {
                return Ok(Some(disambiguated));
            }
            // Hash collision across repos for the same stem. Pathological.
            return Ok(None);
        }
        return Ok(Some(disambiguated));
    }
    Ok(Some(bare))
}

/// Attach all watchers a session needs beyond the per-file plan
/// watcher (which `observe(PlanRegistered)` handles internally via
/// `Watcher::switch_plan_file`): the repo's `.git/logs/HEAD` mover,
/// both feedback subdirectories, and the repo-level `.trinity/plans/`
/// directory. Idempotent.
///
/// Used by all three entry points that bring a session into the
/// active set: explicit `register_plan_file` MCP calls, fresh
/// auto-registers via `PlanDirFileCreated`, and reactivations via
/// `PlanDirFileCreated` against an archived basename. Keeping the
/// three paths on the same helper prevents the reactivation-misses-
/// watcher class of bug.
pub(crate) async fn attach_session_watchers(
    lifecycle: &Arc<SessionService>,
    session_id: &SessionId,
    repo_root: &Path,
) -> anyhow::Result<()> {
    let watcher = lifecycle.watcher();

    // `.git/logs/HEAD` resolution failure is a real activation error
    // (corrupt git layout, unreadable repo). Propagate it so the caller
    // can surface the partial-activation state instead of handing the
    // agent a session that can't observe commits. A missing path
    // (`exists() == false`) is acceptable: fresh repos with no commits
    // yet have no logs/HEAD, and the watcher attaches on the first
    // commit when recovery sweeps the path back in.
    let logs_head = super::daemon::git::resolve_git_logs_head(repo_root).await?;
    if logs_head.exists() {
        watcher.watch_git_logs(session_id, &logs_head)?;
    } else {
        tracing::debug!(
            path = %logs_head.display(),
            "attach_session_watchers: .git/logs/HEAD does not exist yet; attaches on the next commit or re-register"
        );
    }

    for kind in [
        crate::domain::FeedbackKind::Plan,
        crate::domain::FeedbackKind::Impl,
    ] {
        let dir = crate::feedback_path::feedback_dir(repo_root, session_id, kind);
        tokio::fs::create_dir_all(&dir).await?;
        watcher.watch_feedback_dir(session_id, kind, &dir)?;
    }

    let plans_dir = repo_root.join(".trinity").join("plans");
    tokio::fs::create_dir_all(&plans_dir).await?;
    if let Err(err) = watcher.watch_plan_dir(repo_root, &plans_dir) {
        tracing::warn!(
            repo_root = %repo_root.display(),
            error = ?err,
            "attach_session_watchers: plan-dir attach failed"
        );
    }

    Ok(())
}

/// Walk the session's feedback `plan/` and `impl/` directories and
/// re-ingest every `<author>.md` file currently on disk. Used after
/// reactivation: notify mtimes haven't changed since the previous
/// archive, so the ordinary watcher won't fire for these files.
pub(crate) async fn scan_feedback_dir_for_session(
    lifecycle: &Arc<SessionService>,
    repo_root: &Path,
    session_id: &SessionId,
) {
    // Clear `last_observed_hash` on every sidecar row first so the
    // `dispatch_feedback_changed` no-op guard doesn't short-circuit on
    // an unchanged content hash. Active plan has changed; we need to
    // route the same body against the new target.
    if let Err(err) =
        crate::storage::feedback_files::clear_observed_for_session(lifecycle.pool(), session_id)
            .await
    {
        tracing::warn!(
            session_id = session_id.as_str(),
            error = ?err,
            "reactivation drift scan: clear_observed failed"
        );
    }
    let feedback_root = repo_root
        .join(".trinity")
        .join("feedback")
        .join(session_id.as_str());
    for (kind, dir_name) in [
        (crate::domain::FeedbackKind::Plan, "plan"),
        (crate::domain::FeedbackKind::Impl, "impl"),
    ] {
        let dir = feedback_root.join(dir_name);
        let mut read = match tokio::fs::read_dir(&dir).await {
            Ok(d) => d,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = read.next_entry().await {
            let path = entry.path();
            let is_md = path
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("md"))
                .unwrap_or(false);
            if !is_md {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let label = crate::lifecycle::AgentLabel::from(stem);
            if let Err(err) =
                dispatch_feedback_changed(lifecycle, session_id, kind, &label, &path).await
            {
                tracing::warn!(
                    session_id = session_id.as_str(),
                    path = %path.display(),
                    error = ?err,
                    "reactivation drift scan: feedback ingest failed"
                );
            }
        }
    }
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
/// No-op guard is content-based: if the file hash equals the previous
/// observation and the prior ingest did not fail, the call returns early.
/// Target movement alone leaves historical feedback attached to the
/// artifact it was written for; authors must edit the file to apply the
/// same words to a newer target.
async fn dispatch_feedback_changed(
    lifecycle: &Arc<SessionService>,
    session_id: &SessionId,
    feedback_kind: crate::domain::FeedbackKind,
    author_label: &crate::lifecycle::AgentLabel,
    path: &Path,
) -> anyhow::Result<()> {
    use crate::domain::{FeedbackTargetRef, TargetKind};
    use crate::storage::feedback_files;

    let body = match tokio::fs::read_to_string(path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return dispatch_feedback_missing(lifecycle, session_id, feedback_kind, author_label)
                .await;
        }
        Err(e) => return Err(e.into()),
    };
    let hash = blake3::hash(body.as_bytes()).to_hex().to_string();

    // Resolve the kind's expected target. The parse_error sentinel for
    // the None branches lives on `FeedbackTargetResolution`.
    let resolution = lifecycle
        .resolve_target_for_kind(session_id, feedback_kind)
        .await?;

    let now = chrono::Utc::now().timestamp();
    let mut tx = lifecycle.pool().begin().await?;

    let existing =
        feedback_files::fetch(lifecycle.pool(), session_id, feedback_kind, author_label).await?;

    // Same content means same feedback. If HEAD or the latest plan revision
    // moved since the last ingest, keep the prior row anchored to its
    // original artifact until the reviewer edits the file.
    if let Some(row) = existing.as_ref()
        && row.parse_error.is_none()
        && row.last_observed_hash.as_deref() == Some(hash.as_str())
    {
        return Ok(());
    }

    let path_str = path.to_string_lossy().into_owned();
    feedback_files::upsert_observed(
        &mut tx,
        session_id,
        feedback_kind,
        author_label,
        &path_str,
        now,
    )
    .await?;
    feedback_files::set_observed(&mut tx, session_id, feedback_kind, author_label, &hash, now)
        .await?;

    // Phase / kind gate: write parse_error sentinel via the typed
    // resolution; the strings live in `FeedbackTargetResolution::parse_error`.
    let target = match resolution {
        crate::daemon::service::FeedbackTargetResolution::Found(t) => t,
        other => {
            if let Some(sentinel) = other.parse_error() {
                feedback_files::set_parse_error(
                    &mut tx,
                    session_id,
                    feedback_kind,
                    author_label,
                    sentinel,
                    now,
                )
                .await?;
            }
            tx.commit().await?;
            return Ok(());
        }
    };

    if body.trim().is_empty() {
        feedback_files::set_parse_error(
            &mut tx,
            session_id,
            feedback_kind,
            author_label,
            "empty_body",
            now,
        )
        .await?;
        tx.commit().await?;
        return Ok(());
    }

    // Commit the watcher observation before calling put_feedback;
    // put_feedback opens its own transaction and we don't want them
    // entangled.
    tx.commit().await?;

    let target_ref = match target.kind {
        TargetKind::PlanRevision => target
            .id
            .parse::<i64>()
            .ok()
            .map(FeedbackTargetRef::PlanRevision),
        TargetKind::ImplementationCommit => Some(FeedbackTargetRef::ImplementationCommit(
            CommitSha::from(target.id.clone()),
        )),
    };
    let Some(target_ref) = target_ref else {
        return Ok(());
    };

    match lifecycle
        .put_feedback(session_id, author_label, target_ref, body)
        .await
    {
        Ok(_) => {
            let mut tx = lifecycle.pool().begin().await?;
            feedback_files::set_ingested(
                &mut tx,
                feedback_files::Ingested {
                    session_id,
                    kind: feedback_kind,
                    author_label,
                    hash: &hash,
                    target_kind: target.kind,
                    target_id: &target.id,
                    now,
                },
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
                feedback_kind,
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

/// Feedback file vanished from disk. Clear `last_observed_hash` +
/// `parse_error` so derived status flips to `missing`. Historical
/// `feedback` rows are preserved — retraction is v2.
async fn dispatch_feedback_missing(
    lifecycle: &Arc<SessionService>,
    session_id: &SessionId,
    feedback_kind: crate::domain::FeedbackKind,
    author_label: &crate::lifecycle::AgentLabel,
) -> anyhow::Result<()> {
    use crate::storage::feedback_files;
    let now = chrono::Utc::now().timestamp();
    let mut tx = lifecycle.pool().begin().await?;
    feedback_files::mark_missing(&mut tx, session_id, feedback_kind, author_label, now).await?;
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

fn expand_home(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(s)
}
