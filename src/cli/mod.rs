//! Operator-facing CLI commands. The daemon never writes Trinity
//! artifacts; this module does, via local `git` subprocess calls
//! and direct filesystem writes. Mutations live here; the daemon
//! exposes typed read-only previews (`finish_preview`,
//! `rewrite_preview`) that drive what each command actually does.
//!
//! See `.trinity/plans/trinity-cli.md` for the contract.

use clap::Args;

pub mod init;

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<std::path::PathBuf>,
}

#[derive(Args, Debug)]
pub struct FinishArgs {
    /// Plan to finalize. Accepts `<repo>/<stem>.md` or just `<stem>`.
    /// Optional when the cwd-repo has exactly one in-flight active plan.
    pub plan: Option<String>,
}

#[derive(Args, Debug)]
pub struct PurgeArgs {
    /// Plan to purge. Accepts `<repo>/<stem>.md` or just `<stem>`.
    pub plan: Option<String>,
}

pub async fn finish(_args: FinishArgs) -> anyhow::Result<()> {
    anyhow::bail!("trinity finish is not yet implemented (lands in Phase 2)")
}

pub async fn purge(_args: PurgeArgs) -> anyhow::Result<()> {
    anyhow::bail!("trinity purge is not yet implemented (lands in Phase 4)")
}
