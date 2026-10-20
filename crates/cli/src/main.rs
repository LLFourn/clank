use clank::cli::command::{Cli, dispatch, exit_code_for};
use clap::FromArgMatches;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Reset SIGPIPE to default so piping into `head` etc. doesn't
    // panic on broken pipe.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    init_tracing();
    let cli = Cli::from_arg_matches(&clank::cli::command::cli().get_matches())
        .unwrap_or_else(|e| e.exit());
    if let Err(e) = dispatch(cli).await {
        let code = exit_code_for(&e);
        eprintln!("{e:#}");
        clank::cli::source_group::exit(code);
    }
    Ok(())
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
