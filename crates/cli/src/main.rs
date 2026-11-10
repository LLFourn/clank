use clank::cli::command::{Cli, dispatch, exit_code_for};
use clap::FromArgMatches;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    reset_sigpipe();
    init_tracing();
    let cli = Cli::from_arg_matches(&clank::cli::command::cli().get_matches())
        .unwrap_or_else(|e| e.exit());
    // Here and not in the library: a test drives the same commands from a
    // binary that is nobody's installation.
    if cli.command.outlives_an_install() {
        clank::upgrade::arm();
    }
    if let Err(e) = dispatch(cli).await {
        let code = exit_code_for(&e);
        eprintln!("{e:#}");
        clank::cli::source_group::exit(code);
    }
    Ok(())
}

/// Restore SIGPIPE's default, so piping into `head` ends clank quietly
/// rather than panicking on a broken pipe.
///
/// Unsafe because Rust sets SIGPIPE to ignore and has no safe API to undo
/// that.
#[allow(unsafe_code)]
fn reset_sigpipe() {
    // SAFETY: `signal` takes no pointers, and the default disposition
    // runs no handler code; a disposition change is atomic for every
    // thread, so the runtime's workers see either the old or the new one.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
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
