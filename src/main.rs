use clap::{Parser, Subcommand};
use trinity::{daemon, mcp_shim};

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
    /// Run the Trinity daemon (HTTP UI + internal API + watcher).
    Serve(daemon::ServeArgs),
    /// stdio MCP server. Forwards tool calls to a running `trinity serve` daemon.
    Mcp(mcp_shim::McpArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Command::Serve(args) => daemon::serve(args).await,
        Command::Mcp(args) => mcp_shim::run(args).await,
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
