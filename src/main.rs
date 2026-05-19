use clap::{Parser, Subcommand};
use trinity::{cli, mcp_shim, server};

#[derive(Parser)]
#[command(
    name = "trinity",
    version,
    about = "Multi-agent peer review around watched artifacts"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the Trinity daemon (HTTP UI + internal API + filesystem watcher).
    Serve(server::ServeArgs),
    /// stdio MCP server. Forwards tool calls to a running `trinity serve` daemon.
    Mcp(mcp_shim::McpArgs),
    /// Scaffold `.trinity/` in a repo (creates `plans/` and `.gitignore`).
    Init(cli::InitArgs),
    /// Finalize an approved plan: seal the approving feedback into
    /// `.trinity/finished/<stem>/` and commit.
    Finish(cli::FinishArgs),
    /// Strip a plan's `.trinity/` artifacts from history.
    Purge(cli::PurgeArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli_args = Cli::parse();
    match cli_args.command {
        Command::Serve(args) => server::serve(args).await,
        Command::Mcp(args) => mcp_shim::run(args).await,
        Command::Init(args) => cli::init::run(args).await,
        Command::Finish(args) => cli::finish(args).await,
        Command::Purge(args) => cli::purge(args).await,
    }
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("trinity=info,warn"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
