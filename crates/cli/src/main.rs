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
    /// Finish a plan: move its file from `plans/` to `finished/`.
    Finish(cli::FinishArgs),
    /// Unfinish a plan: move it back from `finished/` to `plans/`.
    Unfinish(cli::UnfinishArgs),
    /// Strip a plan's `.clank/` artifacts from history.
    Purge(cli::PurgeArgs),
    /// Print a chronological timeline of commits and reviews for
    /// a plan.
    Log(cli::LogArgs),
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
    /// Stop-hook adapter: reads HookInput JSON from stdin and
    /// emits per-tool continuation output. Installed into the
    /// agent's Stop hook config by `clank setup`; not typically
    /// invoked directly.
    StopHook(cli::StopHookArgs),
    /// Install user-scope clank assets into ~/.claude and
    /// ~/.codex: skill files, slash command, and Stop hook
    /// entries tag-merged into the per-tool config files.
    Setup(cli::SetupArgs),
    /// Check that the clank integration is correctly set up
    /// across repo, user, and current-session scopes. Exits 0
    /// if everything is OK/Warn; 1 if anything is Fail.
    Doctor(cli::DoctorArgs),
    /// Read or write Clank config values.
    Config(cli::ConfigArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Reset SIGPIPE to default so piping into `head` etc. doesn't
    // panic on broken pipe.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    init_tracing();
    let cli_args = Cli::parse();
    let result = match cli_args.command {
        Command::Init(args) => cli::init::run(args).await,
        Command::Finish(args) => cli::finish::run(args).await,
        Command::Unfinish(args) => cli::unfinish::run(args).await,
        Command::Purge(args) => cli::purge::run(args).await,
        Command::Log(args) => cli::log::run(args).await,
        Command::Status(args) => cli::status::run(args).await,
        Command::Wfw(args) => cli::wfw::run(args).await,
        Command::Feedback(args) => cli::feedback::run(args).await,
        Command::As(args) => cli::as_cmd::run(args).await,
        Command::Auto(args) => cli::auto::run(args).await,
        Command::StopHook(args) => cli::stop_hook::run(args).await,
        Command::Setup(args) => cli::setup::run(args).await,
        Command::Doctor(args) => cli::doctor::run(args).await,
        Command::Config(args) => cli::config::run(args).await,
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
    if err.downcast_ref::<cli::doctor::DoctorFailed>().is_some() {
        return 1;
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
