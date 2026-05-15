//! Shared state passed to axum handlers.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::runtime::Runtime;

#[derive(Clone)]
pub struct AppState {
    pub runtime: Arc<Runtime>,
    /// Notify watcher join handles, keyed by canonical repo root so
    /// `delete_repo` can target one and `start_plan` can avoid
    /// double-watching. Dropped when the server stops; tasks abort
    /// with the runtime.
    pub watchers: Arc<Mutex<BTreeMap<PathBuf, tokio::task::JoinHandle<()>>>>,
    /// Directory containing the built Leptos bundle. Served at `/static/*`.
    pub frontend_dist: PathBuf,
    /// `frontend_dist/index.html` read once at boot. The SPA fallback
    /// serves this from memory rather than touching disk on every
    /// request. `None` if the bundle was missing at startup — fallback
    /// returns 503 with a hint to run `trunk build`.
    pub spa_shell: Option<Arc<String>>,
    /// Expanded path of the repos registry file (`--repos` /
    /// `$TRINITY_REPOS`, defaulting to `~/.trinity/repos`). Both
    /// `start_plan` (persist) and the new `delete_repo` handler write
    /// against this path instead of hardcoding `~/.trinity/repos`.
    pub repos_path: PathBuf,
}
