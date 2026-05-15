//! Shared state passed to axum handlers.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::runtime::Runtime;

#[derive(Clone)]
pub struct AppState {
    pub runtime: Arc<Runtime>,
    /// Notify watcher join handles, kept alive for the lifetime of the
    /// server. Dropped when the server stops; tasks abort with the runtime.
    pub watchers: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    /// Set of repo roots that already have a notify watcher attached.
    /// Used by `start_plan` to avoid double-watching when a new repo is
    /// registered mid-session.
    pub watched_repos: Arc<Mutex<HashSet<PathBuf>>>,
    /// Directory containing the built Leptos bundle. Served at `/static/*`.
    pub frontend_dist: PathBuf,
    /// `frontend_dist/index.html` read once at boot. The SPA fallback
    /// serves this from memory rather than touching disk on every
    /// request. `None` if the bundle was missing at startup — fallback
    /// returns 503 with a hint to run `trunk build`.
    pub spa_shell: Option<Arc<String>>,
}
