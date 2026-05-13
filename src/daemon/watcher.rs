//! File-watcher that surfaces "an artifact tied to a session has settled"
//! signals. Three artifact kinds:
//!
//! 1. **Plan files** — a path. Event: `PlanFileDirty` / `PlanFileMissing`.
//! 2. **Git refs** — `.git/logs/HEAD` per repo. Event: `HeadMoved`.
//!    Multiple sessions can share one path (two sessions in the same repo);
//!    we dedupe at the notify-watch level but emit one event per session.
//! 3. **Feedback dirs** — `<repo_root>/.trinity/feedback/<session_id>/<kind>/`
//!    where `<kind>` is either `plan` or `impl`. Event:
//!    `FeedbackFileChanged` / `FeedbackFileMissing` carrying the
//!    `feedback_kind` derived from which watched dir the change landed
//!    in. The filename stem is validated against the slug rules;
//!    files whose names fail validation are silently dropped.
//!
//! The watcher itself is dumb: it tracks `(artifact_kind, path → subscribers)`,
//! runs `notify-debouncer-full`, and emits one `WatcherEvent` per
//! matched subscriber per debounced touch.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use tokio::sync::mpsc;

use crate::domain::FeedbackKind;
use crate::feedback_path::is_valid_slug;
use crate::lifecycle::{AgentLabel, PlanFilePath, SessionId};


#[derive(Debug)]
pub enum WatcherEvent {
    PlanFileDirty {
        session_id: SessionId,
    },
    PlanFileMissing {
        session_id: SessionId,
        path: PathBuf,
    },
    HeadMoved {
        session_id: SessionId,
    },
    FeedbackFileChanged {
        session_id: SessionId,
        feedback_kind: FeedbackKind,
        author_label: AgentLabel,
        path: PathBuf,
    },
    FeedbackFileMissing {
        session_id: SessionId,
        feedback_kind: FeedbackKind,
        author_label: AgentLabel,
        path: PathBuf,
    },
    /// New `.md` file appeared in a repo's `.trinity/plans/` directory.
    /// Carries no `SessionId` — the dispatcher derives one from the
    /// basename and routes through `register_plan_file` (creating or
    /// reactivating the session as needed).
    PlanDirFileCreated {
        repo_root: PathBuf,
        path: PathBuf,
    },
}

struct Inner {
    debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,

    plan_paths: HashMap<PathBuf, Vec<SessionId>>,
    session_to_plan: HashMap<SessionId, PathBuf>,

    git_log_paths: HashMap<PathBuf, Vec<SessionId>>,
    session_to_git_log: HashMap<SessionId, PathBuf>,

    /// Reverse: canonical feedback dir path → subscribers `(session, kind)`.
    feedback_dirs: HashMap<PathBuf, Vec<(SessionId, FeedbackKind)>>,
    /// Forward: `(session, kind)` → canonical dir path. Two entries per
    /// session in normal operation (one plan, one impl).
    session_feedback_dirs: HashMap<(SessionId, FeedbackKind), PathBuf>,

    /// Canonical `.trinity/plans/` dir → owning repo_root. Watched at
    /// startup for known repos so new plan files auto-register.
    plan_dirs: HashMap<PathBuf, PathBuf>,

    /// Per-plan-dir: the set of `.md` file paths we've already seen.
    /// Used to distinguish "file genuinely appeared" (emit
    /// `PlanDirFileCreated`) from "file was modified or atomic-saved"
    /// (notify reports a Create event but the path was already in our
    /// seen set, so no emit). Populated at `watch_plan_dir` attach
    /// time from existing files; updated on subsequent events.
    plan_dir_seen: HashMap<PathBuf, std::collections::HashSet<PathBuf>>,

    notify_refcount: HashMap<PathBuf, usize>,
}

pub struct Watcher {
    inner: Mutex<Inner>,
}

impl Watcher {
    pub fn start(
        debounce_timeout: Duration,
    ) -> anyhow::Result<(std::sync::Arc<Self>, mpsc::UnboundedReceiver<WatcherEvent>)>
    {
        let (event_tx, event_rx) = mpsc::unbounded_channel::<WatcherEvent>();
        let (debouncer_tx, mut debouncer_rx) = mpsc::unbounded_channel::<DebounceEventResult>();

        let debouncer = new_debouncer(debounce_timeout, None, move |res| {
            let _ = debouncer_tx.send(res);
        })?;

        let inner = Mutex::new(Inner {
            debouncer,
            plan_paths: HashMap::new(),
            session_to_plan: HashMap::new(),
            git_log_paths: HashMap::new(),
            session_to_git_log: HashMap::new(),
            feedback_dirs: HashMap::new(),
            session_feedback_dirs: HashMap::new(),
            plan_dirs: HashMap::new(),
            plan_dir_seen: HashMap::new(),
            notify_refcount: HashMap::new(),
        });
        let watcher = std::sync::Arc::new(Self { inner });

        let routed = std::sync::Arc::clone(&watcher);
        tokio::spawn(async move {
            while let Some(result) = debouncer_rx.recv().await {
                match result {
                    Ok(events) => {
                        let emits = {
                            let mut inner = routed.inner.lock().unwrap();
                            collect_emits(&mut inner, &events)
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

    pub fn switch_plan_file(
        &self,
        session_id: &SessionId,
        path: &PlanFilePath,
    ) -> anyhow::Result<()> {
        let canonical = canonicalize_or_warn("plan file", path.as_str()).ok_or_else(|| {
            anyhow::anyhow!(
                "watcher: cannot canonicalize plan-file path {}",
                path.as_str()
            )
        })?;
        let watch_dir = canonical.parent().map(PathBuf::from).ok_or_else(|| {
            anyhow::anyhow!(
                "watcher: plan-file path has no parent: {}",
                canonical.display()
            )
        })?;

        let mut inner = self.inner.lock().unwrap();

        if let Some(prev) = inner.session_to_plan.get(session_id).cloned() {
            if prev == canonical {
                return Ok(());
            }
            drop_plan_subscription(&mut inner, session_id, &prev);
        }

        // Notify attachment must succeed before we record the
        // subscription; otherwise a failure leaves a phantom map entry
        // that makes the next idempotent retry short-circuit on
        // `prev == canonical` without re-attempting notify.
        bump_notify(&mut inner, &watch_dir)?;
        inner
            .plan_paths
            .entry(canonical.clone())
            .or_default()
            .push(session_id.clone());
        inner
            .session_to_plan
            .insert(session_id.clone(), canonical.clone());
        tracing::info!(session_id = session_id.as_str(), path = %canonical.display(), "watcher: now watching plan file");
        Ok(())
    }

    pub fn watch_git_logs(
        &self,
        session_id: &SessionId,
        git_logs_head: &Path,
    ) -> anyhow::Result<()> {
        let canonical = canonicalize_or_warn("git logs/HEAD", git_logs_head).ok_or_else(|| {
            anyhow::anyhow!(
                "watcher: cannot canonicalize git logs/HEAD {}",
                git_logs_head.display()
            )
        })?;
        let watch_dir = canonical.parent().map(PathBuf::from).ok_or_else(|| {
            anyhow::anyhow!(
                "watcher: git logs/HEAD path has no parent: {}",
                canonical.display()
            )
        })?;

        let mut inner = self.inner.lock().unwrap();

        if let Some(prev) = inner.session_to_git_log.get(session_id).cloned() {
            if prev == canonical {
                return Ok(());
            }
            drop_git_log_subscription(&mut inner, session_id, &prev);
        }

        bump_notify(&mut inner, &watch_dir)?;
        inner
            .git_log_paths
            .entry(canonical.clone())
            .or_default()
            .push(session_id.clone());
        inner
            .session_to_git_log
            .insert(session_id.clone(), canonical.clone());
        tracing::info!(session_id = session_id.as_str(), path = %canonical.display(), "watcher: now watching git logs/HEAD");
        Ok(())
    }

    /// Subscribe `(session_id, kind)` to its feedback subdirectory.
    /// Idempotent. A session typically has two subscriptions — one for
    /// `Plan` and one for `Impl`.
    pub fn watch_feedback_dir(
        &self,
        session_id: &SessionId,
        kind: FeedbackKind,
        dir: &Path,
    ) -> anyhow::Result<()> {
        let canonical = canonicalize_or_warn("feedback dir", dir).ok_or_else(|| {
            anyhow::anyhow!(
                "watcher: cannot canonicalize feedback dir {}",
                dir.display()
            )
        })?;

        let mut inner = self.inner.lock().unwrap();
        let key = (session_id.clone(), kind);

        if let Some(prev) = inner.session_feedback_dirs.get(&key).cloned() {
            if prev == canonical {
                return Ok(());
            }
            drop_feedback_dir_subscription(&mut inner, &key, &prev);
        }

        bump_notify(&mut inner, &canonical)?;
        inner
            .feedback_dirs
            .entry(canonical.clone())
            .or_default()
            .push(key.clone());
        inner.session_feedback_dirs.insert(key, canonical.clone());
        tracing::info!(session_id = session_id.as_str(), kind = kind.as_str(), path = %canonical.display(), "watcher: now watching feedback dir");
        Ok(())
    }

    /// Subscribe the canonical `.trinity/plans/` directory for a repo to
    /// the plan-dir watcher. Idempotent. Notify events for `.md` files
    /// here become `WatcherEvent::PlanDirFileCreated` so the dispatcher
    /// can auto-register fresh plan files dropped into the directory.
    pub fn watch_plan_dir(&self, repo_root: &Path, plans_dir: &Path) -> anyhow::Result<()> {
        let canonical = canonicalize_or_warn("plans dir", plans_dir)
            .ok_or_else(|| anyhow::anyhow!("watcher: cannot canonicalize plans dir {}", plans_dir.display()))?;
        let mut inner = self.inner.lock().unwrap();
        if inner.plan_dirs.contains_key(&canonical) {
            return Ok(());
        }
        bump_notify(&mut inner, &canonical)?;
        inner
            .plan_dirs
            .insert(canonical.clone(), repo_root.to_path_buf());
        // Seed the seen set with existing `.md` entries so subsequent
        // notify events on those paths read as modifications, not as
        // fresh drops. A separate "remove + re-create" pair produces a
        // Remove event (which drops the path from this set) followed by
        // a Create — the second event then emits because the path is
        // no longer in the set.
        let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        if let Ok(read) = std::fs::read_dir(&canonical) {
            for entry in read.flatten() {
                let p = entry.path();
                if p.extension()
                    .and_then(|s| s.to_str())
                    .map(|s| s.eq_ignore_ascii_case("md"))
                    .unwrap_or(false)
                {
                    seen.insert(p);
                }
            }
        }
        inner.plan_dir_seen.insert(canonical.clone(), seen);
        tracing::info!(repo_root = %repo_root.display(), path = %canonical.display(), "watcher: now watching plans dir");
        Ok(())
    }

    /// Drop every subscription for `session_id`. Used when a session
    /// is removed.
    #[allow(dead_code)]
    pub fn unwatch_session(&self, session_id: &SessionId) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(p) = inner.session_to_plan.get(session_id).cloned() {
            drop_plan_subscription(&mut inner, session_id, &p);
        }
        if let Some(p) = inner.session_to_git_log.get(session_id).cloned() {
            drop_git_log_subscription(&mut inner, session_id, &p);
        }
        for kind in [FeedbackKind::Plan, FeedbackKind::Impl] {
            let key = (session_id.clone(), kind);
            if let Some(p) = inner.session_feedback_dirs.get(&key).cloned() {
                drop_feedback_dir_subscription(&mut inner, &key, &p);
            }
        }
    }

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

fn collect_emits(
    inner: &mut Inner,
    events: &[notify_debouncer_full::DebouncedEvent],
) -> Vec<WatcherEvent> {
    let mut out: Vec<WatcherEvent> = Vec::new();
    let mut dedupe: Vec<(u8, PathBuf, SessionId, Option<FeedbackKind>)> = Vec::new();

    for ev in events {
        for path in ev.paths.iter() {
            if let Some(sessions) = inner.plan_paths.get(path) {
                let exists = path.exists();
                for sid in sessions {
                    let key = (
                        if exists { 0_u8 } else { 1_u8 },
                        path.clone(),
                        sid.clone(),
                        None,
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

            if let Some(sessions) = inner.git_log_paths.get(path) {
                for sid in sessions {
                    let key = (2_u8, path.clone(), sid.clone(), None);
                    if dedupe.contains(&key) {
                        continue;
                    }
                    dedupe.push(key);
                    out.push(WatcherEvent::HeadMoved {
                        session_id: sid.clone(),
                    });
                }
            }

            if let Some(parent) = path.parent()
                && let Some(repo_root) = inner.plan_dirs.get(parent).cloned()
            {
                let extension_ok = path
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|s| s.eq_ignore_ascii_case("md"))
                    .unwrap_or(false);
                if extension_ok {
                    let parent_buf = parent.to_path_buf();
                    let seen = inner.plan_dir_seen.entry(parent_buf).or_default();
                    if path.exists() {
                        // Only emit when this path is genuinely new
                        // under the watched dir. An already-seen path
                        // with a fresh Create/Modify event means an
                        // in-place edit or atomic-rename save — not a
                        // reactivation trigger.
                        if seen.insert(path.clone()) {
                            let key = (
                                5_u8,
                                path.clone(),
                                SessionId::from(String::new()),
                                None,
                            );
                            if !dedupe.contains(&key) {
                                dedupe.push(key);
                                out.push(WatcherEvent::PlanDirFileCreated {
                                    repo_root,
                                    path: path.clone(),
                                });
                            }
                        }
                    } else {
                        // File vanished. Drop from the seen set so a
                        // subsequent re-create reads as a fresh drop
                        // (auto-register or reactivate path).
                        seen.remove(path);
                    }
                }
            }

            if let Some(parent) = path.parent()
                && let Some(subscribers) = inner.feedback_dirs.get(parent)
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
                for (sid, kind) in subscribers {
                    let key = (
                        if exists { 3_u8 } else { 4_u8 },
                        path.clone(),
                        sid.clone(),
                        Some(*kind),
                    );
                    if dedupe.contains(&key) {
                        continue;
                    }
                    dedupe.push(key);
                    if exists {
                        out.push(WatcherEvent::FeedbackFileChanged {
                            session_id: sid.clone(),
                            feedback_kind: *kind,
                            author_label: label.clone(),
                            path: path.clone(),
                        });
                    } else {
                        out.push(WatcherEvent::FeedbackFileMissing {
                            session_id: sid.clone(),
                            feedback_kind: *kind,
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

fn bump_notify(inner: &mut Inner, dir: &Path) -> Result<(), notify::Error> {
    let count = inner.notify_refcount.entry(dir.to_path_buf()).or_insert(0);
    if *count == 0
        && let Err(e) = inner.debouncer.watch(dir, RecursiveMode::NonRecursive)
    {
        tracing::warn!(dir = %dir.display(), error = ?e, "watcher: failed to add to notify");
        return Err(e);
    }
    *count += 1;
    Ok(())
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

fn drop_feedback_dir_subscription(inner: &mut Inner, key: &(SessionId, FeedbackKind), dir: &Path) {
    if let Some(subscribers) = inner.feedback_dirs.get_mut(dir) {
        subscribers.retain(|k| k != key);
        if subscribers.is_empty() {
            inner.feedback_dirs.remove(dir);
        }
    }
    inner.session_feedback_dirs.remove(key);
    drop_notify(inner, dir);
}
