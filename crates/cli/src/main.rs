use clank::cli;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "clank",
    version,
    about = "Multi-agent peer review around watched artifacts"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scaffold `.clank/` in a repo (creates `plans/` and `.gitignore`).
    Init(cli::InitArgs),
    /// Finalize an approved plan: seal the approving feedback into
    /// `.clank/finished/<stem>/` and commit.
    Finish(cli::FinishArgs),
    /// Strip a plan's `.clank/` artifacts from history.
    Purge(cli::PurgeArgs),
    /// Print the repo's Clank state (HEAD, plans, phases).
    Status(cli::StatusArgs),
    /// Wait-for-work: block until the calling agent has actionable
    /// work on one of the repo's active plans.
    Wfw(cli::WfwArgs),
    /// Read / write feedback files via a typed CLI surface (vs.
    /// editing the on-disk paths directly).
    Feedback(cli::FeedbackArgs),
    /// Bind the calling agent's session (CLAUDE_CODE_SESSION_ID
    /// / CODEX_THREAD_ID env var) to a clank label.
    As(cli::AsArgs),
    /// Manage this agent's auto-mode (Stop-hook behavior) and
    /// optional role designation.
    Auto(cli::AutoArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli_args = Cli::parse();
    let result = match cli_args.command {
        Command::Init(args) => cli::init::run(args).await,
        Command::Finish(args) => cli::finish::run(args).await,
        Command::Purge(args) => cli::purge::run(args).await,
        Command::Status(args) => cli::status::run(args).await,
        Command::Wfw(args) => cli::wfw::run(args).await,
        Command::Feedback(args) => cli::feedback::run(args).await,
        Command::As(args) => cli::as_cmd::run(args).await,
        Command::Auto(args) => cli::auto::run(args).await,
    };
    if let Err(e) = result {
        let code = exit_code_for(&e);
        eprintln!("{e:#}");
        std::process::exit(code);
    }
    Ok(())
}

/// Map error variants to exit codes. Defaults to 1; `wfw` timeout
/// returns 2; status's "ambiguous active plans" returns 3.
fn exit_code_for(err: &anyhow::Error) -> i32 {
    if err.downcast_ref::<cli::wfw::WfwTimeout>().is_some() {
        return 2;
    }
    if let Some(code) = err.downcast_ref::<cli::status::ExitCode>() {
        return code.0;
    }
    1
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("clank=info,warn"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
