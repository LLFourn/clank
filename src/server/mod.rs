//! The Trinity daemon — HTTP server + MCP backend + notify watcher.
//! Backed by the filesystem-truth runtime.

pub mod http;
pub mod mcp;
pub mod notify_bridge;
pub mod state;
pub mod wait;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;
use tokio::sync::Mutex;

use crate::runtime::Runtime;

pub use state::AppState;

#[derive(Args, Debug, Clone)]
pub struct ServeArgs {
    /// Address to bind the HTTP server to. Defaults to 127.0.0.1:7777
    /// (loopback). Do not bind to non-loopback addresses without auth.
    #[arg(long, default_value = "127.0.0.1:7777", env = "TRINITY_BIND")]
    pub bind: SocketAddr,

    /// Path to the repos-list file. One absolute repo root per line.
    /// Trinity loads each line at startup and starts a notify watcher.
    #[arg(long, default_value = "~/.trinity/repos", env = "TRINITY_REPOS")]
    pub repos: String,

    /// Path to the daemon-lock pidfile. Trinity refuses to boot if this
    /// names a still-running PID. Tests override to per-test paths to
    /// avoid contention.
    #[arg(
        long,
        default_value = "~/.trinity/daemon.lock",
        env = "TRINITY_LOCK_FILE"
    )]
    pub lock: String,

    /// Developer escape hatch: serve the Leptos SPA bundle from a
    /// disk directory instead of the binary-embedded one. Set this
    /// when running `trunk watch` in another terminal for fast
    /// frontend iteration without rebuilding the daemon.
    ///
    /// Default (unset) is "use the embedded bundle." The embedded
    /// bundle is rebuilt by `build.rs` whenever frontend source
    /// changes, so version skew between daemon and SPA is impossible
    /// in the default workflow.
    #[arg(long, env = "TRINITY_FRONTEND_DIST")]
    pub frontend_dist: Option<PathBuf>,
}

/// Bundle embedded by `build.rs` running `trunk build --release`.
/// The `include_dir!` macro walks `frontend/dist/` at compile time
/// and bakes every file into the binary.
static EMBEDDED_BUNDLE: include_dir::Dir<'static> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/frontend/dist");

pub async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    // Multi-process safety: refuse to boot if another daemon is alive.
    let lock_guard = acquire_daemon_lock(&args.lock)?;

    let runtime = Arc::new(Runtime::new());

    let repos_path = expand_home(&args.repos);
    let repos = read_repos_file(&repos_path)?;
    if repos.is_empty() {
        tracing::warn!(
            path = %repos_path.display(),
            "no repos configured; create the file with one absolute repo root per line"
        );
    }

    // Track the notify-bridge tasks per repo so we can detach them on drop.
    let mut watchers: BTreeMap<PathBuf, tokio::task::JoinHandle<()>> = BTreeMap::new();
    for repo in repos {
        if !repo.exists() {
            tracing::warn!(path = %repo.display(), "configured repo does not exist; skipping");
            continue;
        }
        let canonical = dunce::canonicalize(&repo).unwrap_or_else(|_| repo.clone());
        match runtime.add_repo(repo.clone()).await {
            Ok(crate::runtime::RegisterOutcome::Registered) => {
                tracing::info!(repo = %repo.display(), "repo loaded")
            }
            Ok(crate::runtime::RegisterOutcome::ShadowedByOther { claimed_by }) => {
                tracing::warn!(
                    repo = %repo.display(),
                    claimed_by = %claimed_by.display(),
                    "repo skipped: basename already claimed by another watched repo",
                );
                continue;
            }
            Err(err) => {
                tracing::error!(repo = %repo.display(), error = ?err, "repo load failed");
                continue;
            }
        }
        match notify_bridge::start(Arc::clone(&runtime), repo.clone()).await {
            Ok(handle) => {
                watchers.insert(canonical, handle);
            }
            Err(err) => {
                tracing::error!(repo = %repo.display(), error = ?err, "watcher start failed");
            }
        }
    }

    let bundle = match &args.frontend_dist {
        Some(path) => {
            tracing::info!(
                path = %path.display(),
                "serving SPA bundle from disk (frontend dev mode)"
            );
            state::Bundle::Disk(path.clone())
        }
        None => state::Bundle::Embedded(&EMBEDDED_BUNDLE),
    };
    let state = AppState {
        runtime: Arc::clone(&runtime),
        watchers: Arc::new(Mutex::new(watchers)),
        bundle,
        repos_path: repos_path.clone(),
    };

    let app = http::router(state);
    tracing::info!(bind = %args.bind, "trinity listening (filesystem-truth)");
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    axum::serve(listener, app).await?;
    drop(lock_guard);
    Ok(())
}

/// Lightweight pidfile-based lock at `~/.trinity/daemon.lock`. Refuses to
/// boot if the file names a still-running pid. Removes the file on drop.
struct DaemonLock {
    path: PathBuf,
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn acquire_daemon_lock(path_str: &str) -> anyhow::Result<DaemonLock> {
    let path = expand_home(path_str);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let pid_str = existing.trim();
        if let Ok(pid) = pid_str.parse::<i32>()
            && is_pid_alive(pid)
        {
            anyhow::bail!(
                "another trinity daemon is running (pid {pid}); refusing to start. \
                 If you are sure no daemon is running, remove {}.",
                path.display()
            );
        }
        // Stale lock from a crashed/killed daemon — overwrite.
    }
    let pid = std::process::id();
    std::fs::write(&path, format!("{pid}\n"))?;
    Ok(DaemonLock { path })
}

fn is_pid_alive(pid: i32) -> bool {
    // `kill -0 <pid>` succeeds iff the pid exists. Unix-only — for
    // Trinity's macOS/Linux scope, that's fine.
    use std::process::Command;
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn read_repos_file(path: &std::path::Path) -> std::io::Result<Vec<PathBuf>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let body = std::fs::read_to_string(path)?;
    Ok(body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(PathBuf::from)
        .collect())
}

fn expand_home(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(s)
}
