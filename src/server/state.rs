//! Shared state passed to axum handlers.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::runtime::Runtime;

#[derive(Clone)]
pub struct AppState {
    pub runtime: Arc<Runtime>,
    /// Notify watcher join handles, kept alive for the lifetime of the
    /// server. Dropped when the server stops; tasks abort with the runtime.
    pub _watchers: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}
