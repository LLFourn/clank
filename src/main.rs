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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli_args = Cli::parse();
    match cli_args.command {
        Command::Init(args) => cli::init::run(args).await,
        Command::Finish(args) => cli::finish::run(args).await,
        Command::Purge(args) => cli::purge::run(args).await,
        Command::Status(args) => cli::status::run(args).await,
    }
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
