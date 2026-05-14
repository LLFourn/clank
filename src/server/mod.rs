//! The Trinity daemon — HTTP server + MCP backend + notify watcher.
//! Backed by the filesystem-truth runtime.

pub mod http;
pub mod mcp;
pub mod notify_bridge;
pub mod state;
pub mod ui;

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
    #[arg(
        long,
        default_value = "~/.trinity/repos",
        env = "TRINITY_REPOS"
    )]
    pub repos: String,
}

pub async fn serve(args: ServeArgs) -> anyhow::Result<()> {
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
    let mut watchers = Vec::with_capacity(repos.len());
    for repo in repos {
        if !repo.exists() {
            tracing::warn!(path = %repo.display(), "configured repo does not exist; skipping");
            continue;
        }
        match runtime.add_repo(repo.clone()).await {
            Ok(()) => tracing::info!(repo = %repo.display(), "repo loaded"),
            Err(err) => {
                tracing::error!(repo = %repo.display(), error = ?err, "repo load failed");
                continue;
            }
        }
        match notify_bridge::start(Arc::clone(&runtime), repo.clone()).await {
            Ok(handle) => watchers.push(handle),
            Err(err) => tracing::error!(repo = %repo.display(), error = ?err, "watcher start failed"),
        }
    }

    let state = AppState {
        runtime: Arc::clone(&runtime),
        _watchers: Arc::new(Mutex::new(watchers)),
    };

    let app = http::router(state);
    tracing::info!(bind = %args.bind, "trinity listening (filesystem-truth)");
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
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
