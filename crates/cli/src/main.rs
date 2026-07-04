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
    /// Manage the plan queue.
    Queue(cli::QueueArgs),
    /// Manage THIS repo's agent ROSTER (the operating list):
    /// `agent add` / `agent promote` / `agent remove` /
    /// `agent list` / `agent start`. Each agent carries its tool +
    /// launch profile AND role; `agent add <name>` adds by name from
    /// the global library, or `--tool` defines one inline.
    Agent(cli::AgentArgs),
    /// Manage the GLOBAL team-template library (a team is a saved
    /// roster): `team save` (capture this repo's roster as a
    /// template) / `team list` / `team show <name>` / `team delete`.
    /// Seed a repo from a template with `clank init --team <name>`.
    Team(cli::TeamArgs),
    /// Serialize this repo's self-contained config (the `agents`
    /// roster) as pretty JSON to stdout. Fail-closed on the old shape
    /// (re-init hint).
    Export(cli::ExportArgs),
    /// Strip a plan's `.clank/` artifacts from history.
    Purge(cli::PurgeArgs),
    /// Copy plans (their commits + plan files) from another branch onto
    /// the current one. The source is never modified; run it where you
    /// want the plans to land.
    Pick(cli::PickArgs),
    /// Launch the configured editor on a plan or commit-range
    /// diff. See `clank diff --help` for argument shapes.
    Diff(cli::DiffArgs),
    /// Set an in-flight plan's commits aside and restore them later,
    /// with git-stash verbs: `stash push` / `stash pop` / `stash show`
    /// / `stash drop`; bare `clank stash` lists. `push --to-queue`
    /// also saves the plan body back to the queue for re-attempt.
    Stash(cli::StashArgs),
    /// Hidden alias for `clank stash push` (+ `shelve clean` →
    /// `stash drop`). One release of back-compat.
    #[command(hide = true)]
    Shelve(cli::ShelveArgs),
    /// Create a linked worktree with the whole team's sessions
    /// forked into it (opens a tab when inside zellij)
    Fork(cli::ForkArgs),
    /// Run the multi-agent review loop against a GitHub PR.
    PrReview(cli::PrReviewArgs),
    /// Hidden alias for `clank stash pop`. One release of back-compat.
    #[command(hide = true)]
    Unshelve(cli::UnshelveArgs),
    /// Print a chronological timeline of commits and reviews for
    /// a plan.
    Log(cli::LogArgs),
    /// Render the event log + status to a static HTML site at
    /// `.clank/html/`. Use `clank html open` to build and
    /// launch the result in your browser.
    Html(cli::HtmlArgs),
    /// Open your agent workspace, context-aware: the self-managed
    /// console in a fresh terminal, or a zellij tab when you're
    /// already in a session. `clank open zellij` forces the zellij
    /// layout; `clank open dry <path>` is the read-only path
    /// classifier.
    Open(cli::OpenArgs),
    /// Rewire feedback files after a rebase / amend. Installed
    /// as a `post-rewrite` git hook by `clank init`; reads
    /// old->new SHA pairs from stdin (with --from-stdin).
    Rewire(cli::RewireArgs),
    /// Print the repo's Clank state (HEAD, plans, phases).
    Status(cli::StatusArgs),
    /// Wait-for-work: block until the calling agent has actionable
    /// work on one of the repo's active plans.
    Wait(cli::WaitArgs),
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
    /// ~/.codex: skill files and Stop hook entries.
    Setup(cli::SetupArgs),
    /// Check that the clank integration is correctly set up
    /// across repo, user, and current-session scopes. Exits 0
    /// if everything is OK/Warn; 1 if anything is Fail.
    Doctor(cli::DoctorArgs),
    /// Read or write Clank config values.
    Config(cli::ConfigArgs),
    /// Create or clean blocks.
    Block(cli::BlockArgs),
    /// Answer a pending block.
    Unblock(cli::UnblockArgs),
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
        Command::Queue(args) => cli::queue::run(args).await,
        Command::Pick(args) => cli::pick::run(args).await,
        Command::Agent(args) => cli::agent::run(args).await,
        Command::Team(args) => cli::team::run(args).await,
        Command::Export(args) => cli::export::run(args).await,
        Command::Purge(args) => cli::purge::run(args).await,
        Command::Diff(args) => cli::diff::run(args).await,
        Command::Stash(args) => cli::stash::run(args).await,
        Command::Shelve(args) => cli::stash::run_shelve_alias(args).await,
        Command::Unshelve(args) => cli::stash::run_unshelve_alias(args).await,
        Command::Fork(args) => cli::fork::run(args).await,
        Command::PrReview(args) => cli::pr_review::run(args).await,
        Command::Log(args) => cli::log::run(args).await,
        Command::Html(args) => cli::html::run(args).await,
        Command::Open(args) => cli::open::run(args).await,
        Command::Rewire(args) => cli::rewire::run(args).await,
        Command::Status(args) => cli::status::run(args).await,
        Command::Wait(args) => cli::wait::run(args).await,
        Command::Feedback(args) => cli::feedback::run(args).await,
        Command::As(args) => cli::as_cmd::run(args).await,
        Command::Auto(args) => cli::auto::run(args).await,
        Command::StopHook(args) => cli::stop_hook::run(args).await,
        Command::Setup(args) => cli::setup::run(args).await,
        Command::Doctor(args) => cli::doctor::run(args).await,
        Command::Config(args) => cli::config::run(args).await,
        Command::Block(args) => cli::block::run(args).await,
        Command::Unblock(args) => cli::block::run_unblock(args).await,
    };
    if let Err(e) = result {
        let code = exit_code_for(&e);
        eprintln!("{e:#}");
        std::process::exit(code);
    }
    Ok(())
}

/// Map error variants to exit codes. Defaults to 1; `wait` timeout
/// returns 2; status's "ambiguous active plans" returns 3.
fn exit_code_for(err: &anyhow::Error) -> i32 {
    if err.downcast_ref::<cli::wait::WaitTimeout>().is_some() {
        return 2;
    }
    if err.downcast_ref::<cli::doctor::DoctorFailed>().is_some() {
        return 1;
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_command_parses() {
        assert!(matches!(
            Cli::try_parse_from(["clank", "wait"]).unwrap().command,
            Command::Wait(_)
        ));
    }

    /// Pin the EXACT argv the TUI spawns to open a page (parent `html` flags
    /// before the `open` subcommand). Exercises the real builder — a
    /// flags-after-subcommand regression fails here at build time instead of
    /// silently at runtime (the TUI spawns detached with nulled output).
    #[test]
    fn html_open_argv_parses() {
        use cli::html::{HtmlOpenTarget, html_open_argv};
        use cli::{HtmlCmd, HtmlOpenArgs};
        use std::path::Path;

        let parse = |argv: Vec<String>| {
            Cli::try_parse_from(std::iter::once("clank".to_string()).chain(argv))
                .expect("spawned argv must parse")
                .command
        };

        // Plan target → open <stem>, --repo/--quiet on the parent.
        match parse(html_open_argv(
            Path::new("/r"),
            HtmlOpenTarget::Plan("foo"),
            false,
        )) {
            Command::Html(h) => {
                assert_eq!(h.repo.as_deref(), Some(Path::new("/r")));
                assert!(h.quiet);
                assert!(!h.rebuild);
                let Some(HtmlCmd::Open(HtmlOpenArgs { plan, commit, .. })) = h.command else {
                    panic!("expected html open");
                };
                assert_eq!(plan.as_deref(), Some("foo"));
                assert_eq!(commit, None);
            }
            _ => panic!("expected html command"),
        }

        // Commit target with rebuild → --rebuild before `open --commit <sha>`.
        match parse(html_open_argv(
            Path::new("/r"),
            HtmlOpenTarget::Commit("abc123"),
            true,
        )) {
            Command::Html(h) => {
                assert!(h.rebuild);
                let Some(HtmlCmd::Open(HtmlOpenArgs { plan, commit, .. })) = h.command else {
                    panic!("expected html open");
                };
                assert_eq!(plan, None);
                assert_eq!(commit.as_deref(), Some("abc123"));
            }
            _ => panic!("expected html command"),
        }

        // Stash target -> `open --stash <name>`.
        match parse(html_open_argv(
            Path::new("/r"),
            HtmlOpenTarget::Stash("parked"),
            false,
        )) {
            Command::Html(h) => {
                let Some(HtmlCmd::Open(HtmlOpenArgs { stash, .. })) = h.command else {
                    panic!("expected html open");
                };
                assert_eq!(stash.as_deref(), Some("parked"));
            }
            _ => panic!("expected html command"),
        }

        // Queue target -> `open --queue <name>`.
        match parse(html_open_argv(
            Path::new("/r"),
            HtmlOpenTarget::Queue("bar"),
            false,
        )) {
            Command::Html(h) => {
                let Some(HtmlCmd::Open(HtmlOpenArgs {
                    plan,
                    commit,
                    queue,
                    ..
                })) = h.command
                else {
                    panic!("expected html open");
                };
                assert_eq!(plan, None);
                assert_eq!(commit, None);
                assert_eq!(queue.as_deref(), Some("bar"));
            }
            _ => panic!("expected html command"),
        }
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
