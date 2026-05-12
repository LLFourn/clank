//! File-watcher that surfaces "an artifact tied to a session has settled"
//! signals. Three artifact kinds:
//!
//! 1. **Plan files** — a path. Event: `PlanFileDirty` / `PlanFileMissing`.
//! 2. **Git refs** — `.git/logs/HEAD` per repo. Event: `HeadMoved`.
//!    Multiple sessions can share one path (two sessions in the same repo);
//!    we dedupe at the notify-watch level but emit one event per session.
//! 3. **Feedback dirs** — `<repo_root>/.trinity/feedback/<session_id>/`.
//!    Event: `FeedbackFileChanged` / `FeedbackFileMissing`. The filename
//!    stem is validated against the slug rules; files whose names fail
//!    validation are silently dropped at the watcher layer.
//!
//! The watcher itself is dumb: it tracks `(artifact_kind, path → sessions)`,
//! runs `notify-debouncer-full`, and emits one `WatcherEvent` per
//! matched session per debounced touch.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use tokio::sync::mpsc;

use crate::lifecycle::{AgentLabel, PlanFilePath, SessionId};

pub const DEBOUNCE_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Debug)]
pub enum WatcherEvent {
    /// Watched plan file for this session has changed (post-debounce).
    PlanFileDirty { session_id: SessionId },
    /// Watched plan file for this session has vanished on disk.
    PlanFileMissing {
        session_id: SessionId,
        path: PathBuf,
    },
    /// `.git/logs/HEAD` for the repo this session lives in has moved.
    /// The dispatcher reads HEAD via `git rev-parse` to discover the
    /// new SHA and feeds the lifecycle a `CommitObserved`.
    HeadMoved { session_id: SessionId },
    /// A markdown file `<author_label>.md` under this session's
    /// feedback directory changed.
    FeedbackFileChanged {
        session_id: SessionId,
        author_label: AgentLabel,
        path: PathBuf,
    },
    /// The same file vanished. The dispatcher marks the sidecar
    /// missing; historical feedback rows are preserved.
    FeedbackFileMissing {
        session_id: SessionId,
        author_label: AgentLabel,
        path: PathBuf,
    },
}

struct Inner {
    debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,

    /// Canonical plan-file path → set of session_ids subscribed to it.
    plan_paths: HashMap<PathBuf, Vec<SessionId>>,
    session_to_plan: HashMap<SessionId, PathBuf>,

    /// Canonical `.git/logs/HEAD` path → set of session_ids in that repo.
    git_log_paths: HashMap<PathBuf, Vec<SessionId>>,
    session_to_git_log: HashMap<SessionId, PathBuf>,

    /// Canonical feedback directory path → set of session_ids.
    /// (One session per dir in practice, but the map is general.)
    feedback_dirs: HashMap<PathBuf, Vec<SessionId>>,
    session_to_feedback_dir: HashMap<SessionId, PathBuf>,

    /// Refcount of paths registered with notify, keyed by the
    /// directory we actually `watch()`. Plan files: parent dir of the
    /// file. Git logs: parent dir (`.git/logs/`). Feedback: the dir
    /// itself.
    notify_refcount: HashMap<PathBuf, usize>,
}

pub struct Watcher {
    inner: Mutex<Inner>,
}

impl Watcher {
    pub fn start() -> anyhow::Result<(std::sync::Arc<Self>, mpsc::UnboundedReceiver<WatcherEvent>)>
    {
        let (event_tx, event_rx) = mpsc::unbounded_channel::<WatcherEvent>();
        let (debouncer_tx, mut debouncer_rx) = mpsc::unbounded_channel::<DebounceEventResult>();

        let debouncer = new_debouncer(DEBOUNCE_TIMEOUT, None, move |res| {
            let _ = debouncer_tx.send(res);
        })?;

        let inner = Mutex::new(Inner {
            debouncer,
            plan_paths: HashMap::new(),
            session_to_plan: HashMap::new(),
            git_log_paths: HashMap::new(),
            session_to_git_log: HashMap::new(),
            feedback_dirs: HashMap::new(),
            session_to_feedback_dir: HashMap::new(),
            notify_refcount: HashMap::new(),
        });
        let watcher = std::sync::Arc::new(Self { inner });

        let routed = std::sync::Arc::clone(&watcher);
        tokio::spawn(async move {
            while let Some(result) = debouncer_rx.recv().await {
                match result {
                    Ok(events) => {
                        let emits = {
                            let inner = routed.inner.lock().unwrap();
                            collect_emits(&inner, &events)
                        };
                        for ev in emits {
                            let _ = event_tx.send(ev);
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

        Ok((watcher, event_rx))
    }

    /// Subscribe `session_id` to a plan file path. Idempotent; if the
    /// session was previously watching a different plan path, the old
    /// binding is dropped first.
    pub fn switch_plan_file(&self, session_id: &SessionId, path: &PlanFilePath) {
        let Some(canonical) = canonicalize_or_warn("plan file", path.as_str()) else {
            return;
        };
        let Some(watch_dir) = canonical.parent().map(PathBuf::from) else {
            tracing::warn!(path = %canonical.display(), "watcher: plan-file path has no parent");
            return;
        };

        let mut inner = self.inner.lock().unwrap();

        if let Some(prev) = inner.session_to_plan.get(session_id).cloned() {
            if prev == canonical {
                return;
            }
            drop_plan_subscription(&mut inner, session_id, &prev);
        }

        inner
            .plan_paths
            .entry(canonical.clone())
            .or_default()
            .push(session_id.clone());
        inner
            .session_to_plan
            .insert(session_id.clone(), canonical.clone());
        bump_notify(&mut inner, &watch_dir);
        tracing::info!(session_id = session_id.as_str(), path = %canonical.display(), "watcher: now watching plan file");
    }

    /// Subscribe `session_id` to its repo's `.git/logs/HEAD`. Idempotent;
    /// multi-session sharing is fine — the watcher dedupes at notify level
    /// and emits one `HeadMoved` per session.
    pub fn watch_git_logs(&self, session_id: &SessionId, git_logs_head: &Path) {
        let Some(canonical) = canonicalize_or_warn("git logs/HEAD", git_logs_head) else {
            return;
        };
        let Some(watch_dir) = canonical.parent().map(PathBuf::from) else {
            tracing::warn!(path = %canonical.display(), "watcher: git logs/HEAD path has no parent");
            return;
        };

        let mut inner = self.inner.lock().unwrap();

        if let Some(prev) = inner.session_to_git_log.get(session_id).cloned() {
            if prev == canonical {
                return;
            }
            drop_git_log_subscription(&mut inner, session_id, &prev);
        }

        inner
            .git_log_paths
            .entry(canonical.clone())
            .or_default()
            .push(session_id.clone());
        inner
            .session_to_git_log
            .insert(session_id.clone(), canonical.clone());
        bump_notify(&mut inner, &watch_dir);
        tracing::info!(session_id = session_id.as_str(), path = %canonical.display(), "watcher: now watching git logs/HEAD");
    }

    /// Subscribe `session_id` to its feedback directory.
    /// `<repo_root>/.trinity/feedback/<session_id>/`. Idempotent.
    pub fn watch_feedback_dir(&self, session_id: &SessionId, dir: &Path) {
        let Some(canonical) = canonicalize_or_warn("feedback dir", dir) else {
            return;
        };

        let mut inner = self.inner.lock().unwrap();

        if let Some(prev) = inner.session_to_feedback_dir.get(session_id).cloned() {
            if prev == canonical {
                return;
            }
            drop_feedback_dir_subscription(&mut inner, session_id, &prev);
        }

        inner
            .feedback_dirs
            .entry(canonical.clone())
            .or_default()
            .push(session_id.clone());
        inner
            .session_to_feedback_dir
            .insert(session_id.clone(), canonical.clone());
        bump_notify(&mut inner, &canonical);
        tracing::info!(session_id = session_id.as_str(), path = %canonical.display(), "watcher: now watching feedback dir");
    }

    /// Drop every subscription for `session_id` (plan file + git logs
    /// + feedback dir). Used when a session is removed.
    #[allow(dead_code)]
    pub fn unwatch_session(&self, session_id: &SessionId) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(p) = inner.session_to_plan.get(session_id).cloned() {
            drop_plan_subscription(&mut inner, session_id, &p);
        }
        if let Some(p) = inner.session_to_git_log.get(session_id).cloned() {
            drop_git_log_subscription(&mut inner, session_id, &p);
        }
        if let Some(p) = inner.session_to_feedback_dir.get(session_id).cloned() {
            drop_feedback_dir_subscription(&mut inner, session_id, &p);
        }
    }

    /// For tests / diagnostics.
    #[allow(dead_code)]
    pub fn plan_path(&self, session_id: &SessionId) -> Option<PathBuf> {
        self.inner
            .lock()
            .unwrap()
            .session_to_plan
            .get(session_id)
            .cloned()
    }
}

/// Translate raw notify events into a flat list of `WatcherEvent`s to
/// emit. Held lock-free outside the closure to keep emission async.
fn collect_emits(inner: &Inner, events: &[notify_debouncer_full::DebouncedEvent]) -> Vec<WatcherEvent> {
    let mut out: Vec<WatcherEvent> = Vec::new();
    let mut dedupe: Vec<(u8, PathBuf, SessionId)> = Vec::new();

    for ev in events {
        for path in ev.paths.iter() {
            // Plan file?
            if let Some(sessions) = inner.plan_paths.get(path) {
                let exists = path.exists();
                for sid in sessions {
                    let key = (
                        if exists { 0_u8 } else { 1_u8 },
                        path.clone(),
                        sid.clone(),
                    );
                    if dedupe.contains(&key) {
                        continue;
                    }
                    dedupe.push(key);
                    if exists {
                        out.push(WatcherEvent::PlanFileDirty {
                            session_id: sid.clone(),
                        });
                    } else {
                        out.push(WatcherEvent::PlanFileMissing {
                            session_id: sid.clone(),
                            path: path.clone(),
                        });
                    }
                }
            }

            // Git logs/HEAD?
            if let Some(sessions) = inner.git_log_paths.get(path) {
                for sid in sessions {
                    let key = (2_u8, path.clone(), sid.clone());
                    if dedupe.contains(&key) {
                        continue;
                    }
                    dedupe.push(key);
                    out.push(WatcherEvent::HeadMoved {
                        session_id: sid.clone(),
                    });
                }
            }

            // Feedback file (path's parent is a watched feedback dir)?
            if let Some(parent) = path.parent()
                && let Some(sessions) = inner.feedback_dirs.get(parent)
            {
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
                if !is_valid_slug(stem) {
                    tracing::debug!(path = %path.display(), "watcher: invalid feedback filename stem; ignored");
                    continue;
                }
                let label = AgentLabel::from(stem);
                let exists = path.exists();
                for sid in sessions {
                    let key = (
                        if exists { 3_u8 } else { 4_u8 },
                        path.clone(),
                        sid.clone(),
                    );
                    if dedupe.contains(&key) {
                        continue;
                    }
                    dedupe.push(key);
                    if exists {
                        out.push(WatcherEvent::FeedbackFileChanged {
                            session_id: sid.clone(),
                            author_label: label.clone(),
                            path: path.clone(),
                        });
                    } else {
                        out.push(WatcherEvent::FeedbackFileMissing {
                            session_id: sid.clone(),
                            author_label: label.clone(),
                            path: path.clone(),
                        });
                    }
                }
            }
        }
    }
    out
}

fn canonicalize_or_warn(label: &str, path: impl AsRef<Path>) -> Option<PathBuf> {
    match dunce::canonicalize(path.as_ref()) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(kind = label, path = %path.as_ref().display(), error = ?e, "watcher: cannot canonicalize");
            None
        }
    }
}

fn bump_notify(inner: &mut Inner, dir: &Path) {
    let count = inner.notify_refcount.entry(dir.to_path_buf()).or_insert(0);
    if *count == 0
        && let Err(e) = inner.debouncer.watch(dir, RecursiveMode::NonRecursive)
    {
        tracing::warn!(dir = %dir.display(), error = ?e, "watcher: failed to add to notify");
        // Leave the refcount at 0 so a later subscription can retry.
        return;
    }
    *count += 1;
}

fn drop_notify(inner: &mut Inner, dir: &Path) {
    let Some(count) = inner.notify_refcount.get_mut(dir) else {
        return;
    };
    *count = count.saturating_sub(1);
    if *count == 0 {
        inner.notify_refcount.remove(dir);
        let _ = inner.debouncer.unwatch(dir);
    }
}

fn drop_plan_subscription(inner: &mut Inner, session_id: &SessionId, path: &Path) {
    if let Some(sessions) = inner.plan_paths.get_mut(path) {
        sessions.retain(|s| s != session_id);
        if sessions.is_empty() {
            inner.plan_paths.remove(path);
        }
    }
    inner.session_to_plan.remove(session_id);
    if let Some(parent) = path.parent() {
        drop_notify(inner, parent);
    }
}

fn drop_git_log_subscription(inner: &mut Inner, session_id: &SessionId, path: &Path) {
    if let Some(sessions) = inner.git_log_paths.get_mut(path) {
        sessions.retain(|s| s != session_id);
        if sessions.is_empty() {
            inner.git_log_paths.remove(path);
        }
    }
    inner.session_to_git_log.remove(session_id);
    if let Some(parent) = path.parent() {
        drop_notify(inner, parent);
    }
}

fn drop_feedback_dir_subscription(inner: &mut Inner, session_id: &SessionId, dir: &Path) {
    if let Some(sessions) = inner.feedback_dirs.get_mut(dir) {
        sessions.retain(|s| s != session_id);
        if sessions.is_empty() {
            inner.feedback_dirs.remove(dir);
        }
    }
    inner.session_to_feedback_dir.remove(session_id);
    drop_notify(inner, dir);
}

/// Same slug rules as `SessionId`: ASCII alphanumerics + `_-.`,
/// 1..=64 chars. Mirrors validate_session_id in the tools layer.
pub fn is_valid_slug(s: &str) -> bool {
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}
