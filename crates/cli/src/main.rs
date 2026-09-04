use clank::cli::command::{Cli, dispatch, exit_code_for};
use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Reset SIGPIPE to default so piping into `head` etc. doesn't
    // panic on broken pipe.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    init_tracing();
    if let Err(e) = dispatch(Cli::parse()).await {
        let code = exit_code_for(&e);
        eprintln!("{e:#}");
        std::process::exit(code);
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
