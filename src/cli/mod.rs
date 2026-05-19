//! Operator-facing CLI commands. The daemon never writes Trinity
//! artifacts; this module does, via local `git` subprocess calls
//! and direct filesystem writes. Mutations live here; the daemon
//! exposes typed read-only previews (`finish_preview`,
//! `rewrite_preview`) that drive what each command actually does.
//!
//! See `.trinity/plans/trinity-cli.md` for the contract.

use clap::Args;
use std::path::{Path, PathBuf};

pub mod finish;
pub mod init;

/// Default daemon URL — matches `trinity serve`'s loopback bind +
/// the MCP shim's default.
pub const DEFAULT_DAEMON: &str = "http://127.0.0.1:7777";

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct FinishArgs {
    /// Plan to finalize. Accepts `<repo>/<stem>.md` or just `<stem>`.
    /// Optional when the cwd-repo has exactly one in-flight active plan.
    pub plan: Option<String>,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Daemon HTTP base URL.
    #[arg(long, default_value = DEFAULT_DAEMON, env = "TRINITY_DAEMON")]
    pub daemon: String,
    /// Amend HEAD instead of creating a new finalize commit. HEAD
    /// must already be a finalize commit for this plan.
    #[arg(long)]
    pub amend: bool,
    /// Override the default `Finalize <stem>` commit message.
    #[arg(short = 'm', long)]
    pub message: Option<String>,
}

#[derive(Args, Debug)]
pub struct PurgeArgs {
    /// Plan to purge. Accepts `<repo>/<stem>.md` or just `<stem>`.
    pub plan: Option<String>,
}

pub async fn purge(_args: PurgeArgs) -> anyhow::Result<()> {
    anyhow::bail!("trinity purge is not yet implemented (lands in Phase 4)")
}

/// Resolve the repo root: explicit `--repo` path wins, otherwise
/// `git rev-parse --show-toplevel` from cwd. Both branches go
/// through `dunce::canonicalize` so macOS `/var → /private/var`
/// symlinks don't yield divergent identities.
pub(crate) fn resolve_repo(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    let raw = if let Some(p) = explicit {
        p.to_path_buf()
    } else {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()?;
        if !output.status.success() {
            anyhow::bail!(
                "no --repo given and `git rev-parse --show-toplevel` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let root = String::from_utf8(output.stdout)?.trim().to_string();
        PathBuf::from(root)
    };
    Ok(dunce::canonicalize(&raw)?)
}

/// Derive the repo basename — the segment Trinity uses to address
/// plans on the wire (`/api/plan/<basename>/<stem>.md`).
pub(crate) fn repo_basename(repo: &Path) -> anyhow::Result<String> {
    repo.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("repo path has no usable basename: {}", repo.display()))
}
