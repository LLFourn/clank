//! Shared state passed to axum handlers.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::runtime::Runtime;

/// Where the daemon reads the Leptos SPA bundle from.
///
/// `Embedded` is the production / default path — `build.rs` ran
/// `trunk build --release` and `include_dir!` baked the result into
/// the binary, so daemon and frontend versions cannot drift.
///
/// `Disk(path)` is a developer escape hatch enabled by the
/// `TRINITY_FRONTEND_DIST_OVERRIDE` env var or `--frontend-dist`
/// CLI flag. Useful when running `trunk watch` in another terminal
/// for fast frontend iteration without rebuilding the daemon.
#[derive(Clone)]
pub enum Bundle {
    Embedded(&'static include_dir::Dir<'static>),
    Disk(PathBuf),
}

#[derive(Clone)]
pub struct AppState {
    pub runtime: Arc<Runtime>,
    /// Notify watcher join handles, keyed by canonical repo root so
    /// `delete_repo` can target one and `start_plan` can avoid
    /// double-watching. Dropped when the server stops; tasks abort
    /// with the runtime.
    pub watchers: Arc<Mutex<BTreeMap<PathBuf, tokio::task::JoinHandle<()>>>>,
    /// Source of the SPA bundle — usually the embedded dist baked
    /// into this binary; optionally a disk path for `trunk watch`
    /// dev loops.
    pub bundle: Bundle,
    /// Expanded path of the repos registry file (`--repos` /
    /// `$TRINITY_REPOS`, defaulting to `~/.trinity/repos`). Both
    /// `start_plan` (persist) and the new `delete_repo` handler write
    /// against this path instead of hardcoding `~/.trinity/repos`.
    pub repos_path: PathBuf,
}
