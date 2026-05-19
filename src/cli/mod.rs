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
pub mod purge;
pub mod rewrite;

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
    #[arg(long, default_value = DEFAULT_DAEMON, env = "TRINITY_DAEMON_URL")]
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
    /// Omit to infer the single active in-flight plan, or pass
    /// `--all` to strip every `.trinity/` path. Mutually exclusive
    /// with `--all`.
    pub plan: Option<String>,
    /// Strip EVERY `.trinity/` path from history (plan files,
    /// finalize snapshots, AND non-plan Trinity metadata like
    /// `.trinity/.gitignore` and `.trinity/stubs/*`). Cannot be
    /// combined with a plan argument.
    #[arg(long)]
    pub all: bool,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Daemon HTTP base URL.
    #[arg(long, default_value = DEFAULT_DAEMON, env = "TRINITY_DAEMON_URL")]
    pub daemon: String,
    /// Write the rewritten chain to a fresh branch instead of
    /// rewriting the current branch in place. Safer — the operator
    /// can inspect / cherry-pick / diff before deciding what to do
    /// with it. Refuses if `<name>` already exists.
    #[arg(long, value_name = "NAME")]
    pub into_branch: Option<String>,
    /// Print the planned rewrite and exit 0 without creating any
    /// commits or moving any refs.
    #[arg(long)]
    pub dry: bool,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Reserved for a future phase: squash plan-attributed
    /// commits into a single commit with the supplied message.
    #[arg(long, value_name = "MSG")]
    pub squash: Option<String>,
    /// Reserved for a future phase: amend HEAD instead of building
    /// a new chain.
    #[arg(long)]
    pub amend: bool,
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
