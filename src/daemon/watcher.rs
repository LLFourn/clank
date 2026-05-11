//! File-watcher that surfaces a "session's plan file has settled" signal.
//!
//! The watcher itself is dumb: it tracks `(session_id → plan_file_path)`,
//! runs `notify-debouncer-full` on each watched file's parent directory,
//! and emits a `WatcherEvent::PlanFileDirty { session_id }` to the dispatch
//! channel. The dispatcher reads the file, captures HEAD, and feeds the
//! observation through `SessionService::observe`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use tokio::sync::mpsc;

use crate::lifecycle::{PlanFilePath, SessionId};

const DEBOUNCE_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Debug)]
pub enum WatcherEvent {
    /// The plan file for this session has changed (post-debounce). The
    /// dispatcher reads the file + captures HEAD and feeds the lifecycle.
    PlanFileDirty { session_id: SessionId },
    /// The plan file for this session has vanished on disk.
    PlanFileMissing {
        session_id: SessionId,
        path: PathBuf,
    },
}

struct Inner {
    debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
    /// Canonical plan-file path → session_id.
    path_to_session: HashMap<PathBuf, SessionId>,
    /// session_id → canonical plan-file path (for switch/unwatch).
    session_to_path: HashMap<SessionId, PathBuf>,
    /// Refcount of watched parent directories.
    parents: HashMap<PathBuf, usize>,
}

pub struct PlanWatcher {
    inner: Mutex<Inner>,
}

impl PlanWatcher {
    /// Build the watcher and return it alongside the receiver the dispatcher
    /// will drain.
    pub fn start() -> anyhow::Result<(std::sync::Arc<Self>, mpsc::UnboundedReceiver<WatcherEvent>)>
    {
        let (event_tx, event_rx) = mpsc::unbounded_channel::<WatcherEvent>();
        let (debouncer_tx, mut debouncer_rx) = mpsc::unbounded_channel::<DebounceEventResult>();

        let debouncer = new_debouncer(DEBOUNCE_TIMEOUT, None, move |res| {
            let _ = debouncer_tx.send(res);
        })?;

        let inner = Mutex::new(Inner {
            debouncer,
            path_to_session: HashMap::new(),
            session_to_path: HashMap::new(),
            parents: HashMap::new(),
        });
        let watcher = std::sync::Arc::new(Self { inner });

        let routed = std::sync::Arc::clone(&watcher);
        tokio::spawn(async move {
            while let Some(result) = debouncer_rx.recv().await {
                match result {
                    Ok(events) => {
                        // Collect unique (path, session) pairs touched in this debounce window.
                        let mut hits: Vec<(SessionId, PathBuf)> = Vec::new();
                        {
                            let inner = routed.inner.lock().unwrap();
                            for ev in events {
                                for path in ev.paths.iter() {
                                    if let Some(sid) = inner.path_to_session.get(path) {
                                        let pair = (sid.clone(), path.clone());
                                        if !hits.contains(&pair) {
                                            hits.push(pair);
                                        }
                                    }
                                }
                            }
                        }
                        for (session_id, path) in hits {
                            // Was-removed check happens at dispatcher time when it reads.
                            // We always emit PlanFileDirty; the dispatcher decides between
                            // "got body" (PlanFileObserved) and "file missing"
                            // (PlanFileMissing event recorded separately).
                            if path.exists() {
                                let _ = event_tx.send(WatcherEvent::PlanFileDirty { session_id });
                            } else {
                                let _ = event_tx
                                    .send(WatcherEvent::PlanFileMissing { session_id, path });
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

        Ok((watcher, event_rx))
    }

    /// Start watching `path` for `session_id`. Idempotent on `(session_id, path)`.
    /// If `session_id` was previously watching a different path, that old
    /// path is implicitly `unwatch`ed first.
    pub fn switch(&self, session_id: &SessionId, path: &PlanFilePath) {
        let canonical = match dunce::canonicalize(path.as_str()) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(path = path.as_str(), error = ?e, "watcher: cannot canonicalize plan-file path");
                return;
            }
        };
        let parent = match canonical.parent() {
            Some(p) => p.to_path_buf(),
            None => {
                tracing::warn!(path = %canonical.display(), "watcher: plan-file path has no parent");
                return;
            }
        };

        let mut inner = self.inner.lock().unwrap();

        // If the same session was already watching the same path, no-op.
        if let Some(prev) = inner.session_to_path.get(session_id) {
            if prev == &canonical {
                return;
            }
            // Different path → unwatch the previous binding.
            let prev = prev.clone();
            unwatch_inner(&mut inner, session_id, &prev);
        }

        let prior_count = inner.parents.get(&parent).copied().unwrap_or(0);
        if prior_count == 0
            && let Err(e) = inner.debouncer.watch(&parent, RecursiveMode::NonRecursive)
        {
            tracing::warn!(parent = %parent.display(), error = ?e, "watcher: failed to watch parent dir");
            return;
        }
        inner.parents.insert(parent, prior_count + 1);
        inner
            .path_to_session
            .insert(canonical.clone(), session_id.clone());
        inner
            .session_to_path
            .insert(session_id.clone(), canonical.clone());
        tracing::info!(session_id = session_id.as_str(), path = %canonical.display(), "watcher: now watching");
    }

    /// Stop watching whatever path `session_id` was bound to. Idempotent.
    pub fn unwatch(&self, session_id: &SessionId) {
        let mut inner = self.inner.lock().unwrap();
        let path = match inner.session_to_path.get(session_id) {
            Some(p) => p.clone(),
            None => return,
        };
        unwatch_inner(&mut inner, session_id, &path);
    }

    /// For startup recovery: the path the watcher currently has bound for a
    /// given session, if any.
    #[allow(dead_code)]
    pub fn current_path(&self, session_id: &SessionId) -> Option<PathBuf> {
        self.inner
            .lock()
            .unwrap()
            .session_to_path
            .get(session_id)
            .cloned()
    }
}

fn unwatch_inner(inner: &mut Inner, session_id: &SessionId, path: &Path) {
    inner.path_to_session.remove(path);
    inner.session_to_path.remove(session_id);
    if let Some(parent) = path.parent()
        && let Some(count) = inner.parents.get_mut(parent)
    {
        *count = count.saturating_sub(1);
        if *count == 0 {
            inner.parents.remove(parent);
            let _ = inner.debouncer.unwatch(parent);
        }
    }
}
