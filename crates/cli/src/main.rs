use clank::cli;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "clank",
    version,
    about = "Multi-agent peer review around plans"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    // ── getting set up ──
    /// Scaffold `.clank/` in a repo
    ///
    /// Creates `plans/` and the managed `.gitignore`, and installs the
    /// `post-rewrite` hook. `--team <name>` seeds the roster from a
    /// saved template.
    Init(cli::InitArgs),
    /// Install skills and hooks into ~/.claude, ~/.codex, ~/.grok
    ///
    /// User-scope assets: the role-split skill files plus Stop-hook
    /// entries for the tools with active hooks (claude, codex).
    /// Re-run after upgrading; `--force` refreshes edited skills.
    Setup(cli::SetupArgs),
    /// Check the clank setup across repo, user, and session
    ///
    /// Per-section OK / WARN / FAIL with actionable messages. Exits 0
    /// if everything is OK or WARN; 1 on any FAIL.
    Doctor(cli::DoctorArgs),

    // ── the daily flow ──
    /// Show the repo's clank state
    ///
    /// HEAD, plans, phases, and the roster. `--watch` renders live,
    /// `--tui` is the full-screen pane, `-j` emits JSON.
    Status(cli::StatusArgs),
    /// Block until this agent has actionable work
    ///
    /// Author and role come from the session binding (`clank as`).
    /// `--peek` probes without blocking; `--for` observes a repo
    /// instead; `--event` adds ad-hoc wake sources (github repos,
    /// commands) alongside the agent config's `wait_events`.
    Wait(cli::WaitArgs),
    /// Manage the plan queue
    ///
    /// `queue add` consumes a draft from `.clank/drafts/`; `promote`
    /// activates the next plan; `remove` / `reprioritise` curate.
    Queue(cli::QueueArgs),
    /// Read or write review feedback
    ///
    /// A typed surface over the on-disk feedback files: `feedback
    /// write --commit <sha> --verdict … -m "…"` and `feedback read`.
    Feedback(cli::FeedbackArgs),
    /// The github event inbox: list, ack, show
    ///
    /// Every github event a wait ingests lands here and re-wakes the
    /// agent until acked. `list` shows the unhandled backlog, `ack`
    /// closes items out, `show` prints one full record.
    Events(cli::EventsArgs),
    /// Print the repo timeline: commits, reviews, github events
    ///
    /// Chronological and newest-first, with github events interleaved
    /// by their github-side time (`--no-github` to leave them out,
    /// `--plan` to scope to one plan, `--oneline` / `-j` for compact
    /// and machine forms).
    Log(cli::LogArgs),
    /// Finalize a plan into `finished/`
    ///
    /// Runs when the gate says FINISHED; `-m` is required (what
    /// changed, then why). With `finish.autosquash` the plan's
    /// commits collapse into one.
    Finish(cli::FinishArgs),
    /// Move a finalized plan back to active
    Unfinish(cli::UnfinishArgs),

    // ── the roster & workspaces ──
    /// Manage this repo's agent roster
    ///
    /// `agent add` / `promote` / `remove` / `list` / `set-review` /
    /// `start`. Each agent carries its tool + launch profile and
    /// role; `agent add <name>` pulls from the global library, or
    /// `--tool` defines one inline.
    Agent(cli::AgentArgs),
    /// Save and reuse roster templates across repos
    ///
    /// `team save` captures this repo's roster; `team list` / `show`
    /// / `delete` manage the library. Seed a repo with
    /// `clank init --team <name>`.
    Team(cli::TeamArgs),
    /// Bind this session to a roster label
    ///
    /// Reads the tool's session id from the environment
    /// (CLAUDE_CODE_SESSION_ID / CODEX_THREAD_ID) and records the
    /// binding in the agent's local config.
    As(cli::AsArgs),
    /// Manage this agent's auto-mode
    ///
    /// `auto on|off|status`, with an optional role designation; the
    /// Stop hook reads it to drive the work loop.
    Auto(cli::AutoArgs),
    /// Open the team workspace
    ///
    /// Context-aware: the self-managed console in a fresh terminal,
    /// or a tab when already inside zellij. `open zellij` forces the
    /// zellij layout; `open dry <path>` classifies without opening.
    Open(cli::OpenArgs),
    /// Fork the team into a linked worktree
    ///
    /// Creates the worktree with the whole team's sessions forked
    /// into it (opens a tab when inside zellij).
    Fork(cli::ForkArgs),

    // ── plan surgery ──
    /// Set a plan's commits aside and restore them later
    ///
    /// git-stash verbs: `stash push` / `pop` / `show` / `drop`; bare
    /// `clank stash` lists. `push --to-queue` re-queues the plan body
    /// for a later attempt.
    Stash(cli::StashArgs),
    /// Hidden alias for `clank stash push` (+ `shelve clean` →
    /// `stash drop`). One release of back-compat.
    #[command(hide = true)]
    Shelve(cli::ShelveArgs),
    /// Hidden alias for `clank stash pop`. One release of back-compat.
    #[command(hide = true)]
    Unshelve(cli::UnshelveArgs),
    /// Copy plans from another branch
    ///
    /// Brings their commits + plan files onto the current branch; the
    /// source is never modified. Run it where the plans should land.
    Pick(cli::PickArgs),
    /// Strip a plan's `.clank/` artifacts from history
    ///
    /// `--drop` removes the plan entirely (commits included).
    Purge(cli::PurgeArgs),
    /// Run the review loop against a GitHub PR
    ///
    /// The team iterates on a shared pending review until it agrees,
    /// then submits it as one published review.
    PrReview(cli::PrReviewArgs),

    // ── odds & ends ──
    /// Open a plan or commit-range diff in your editor
    Diff(cli::DiffArgs),
    /// Render the timeline + status to a static site
    ///
    /// Writes `.clank/html/`; `clank html open` builds and launches
    /// it in your browser.
    Html(cli::HtmlArgs),
    /// Ask the human a blocking question
    Block(cli::BlockArgs),
    /// Answer a pending block
    Unblock(cli::UnblockArgs),
    /// Record the background task you are waiting on, so the Stop hook
    /// stops waking you with work you are already doing
    Attending(cli::AttendingArgs),
    /// Run a command under clank, so it knows exactly when the work ends
    ///
    /// Background this instead of the bare command:
    /// `clank run --desc "test run" -- cargo test`. Clank records what
    /// it is attending, then BECOMES the command — same pid, same
    /// stdio, same exit status — so the harness task's lifetime is the
    /// work's lifetime and nothing has to be told when it finished.
    Run(cli::RunArgs),
    /// Read or write clank config values
    Config(cli::ConfigArgs),
    /// Dump the repo's roster config as JSON
    ///
    /// Pretty-printed to stdout; fails closed on a pre-roster config
    /// shape with a re-init hint.
    Export(cli::ExportArgs),
    /// Re-open review on a plan's latest reviewable commit
    ///
    /// Rewrites it with the same tree and message so it becomes a new
    /// commit that every reviewer owes a fresh verdict on. Use after a
    /// promotion auto-continued a demoted master's self-review.
    Rereview(cli::RereviewArgs),
    /// Remap feedback after a rebase or amend
    ///
    /// Installed as the `post-rewrite` git hook by `clank init`;
    /// reads old→new SHA pairs from stdin with `--from-stdin`.
    Rewire(cli::RewireArgs),
    /// The per-tool Stop-hook adapter
    ///
    /// Reads HookInput JSON on stdin and emits the tool's
    /// continuation output. Installed by `clank setup`; not typically
    /// invoked directly.
    StopHook(cli::StopHookArgs),
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
        Command::Rereview(args) => cli::rereview::run(args).await,
        Command::Rewire(args) => cli::rewire::run(args).await,
        Command::Status(args) => cli::status::run(args).await,
        Command::Wait(args) => cli::wait::run(args).await,
        Command::Feedback(args) => cli::feedback::run(args).await,
        Command::Events(args) => cli::events::run(args).await,
        Command::As(args) => cli::as_cmd::run(args).await,
        Command::Auto(args) => cli::auto::run(args).await,
        Command::StopHook(args) => cli::stop_hook::run(args).await,
        Command::Setup(args) => cli::setup::run(args).await,
        Command::Doctor(args) => cli::doctor::run(args).await,
        Command::Config(args) => cli::config::run(args).await,
        Command::Block(args) => cli::block::run(args).await,
        Command::Unblock(args) => cli::block::run_unblock(args).await,
        Command::Attending(args) => cli::attending::run(args).await,
        Command::Run(args) => cli::run::run(args),
    };
    if let Err(e) = result {
        let code = exit_code_for(&e);
        eprintln!("{e:#}");
        std::process::exit(code);
    }
    Ok(())
}

/// Map error variants to exit codes. Defaults to 1. `wait` no longer
/// has an expected non-zero exit: it parks until a wake, a real
/// error, or owner death (remove-wait-timeout), so exit 2 is free.
fn exit_code_for(err: &anyhow::Error) -> i32 {
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

    #[test]
    fn wait_for_parses_conflicts_with_peek_and_tolerates_author() {
        // wait-for-observer-mode: each event parses; --peek conflicts
        // (a delta probe has no baseline); --author is ACCEPTED
        // alongside --for (documented as ignored, not an error).
        for (raw, want) in [
            ("commit", cli::WaitFor::Commit),
            ("finished", cli::WaitFor::Finished),
            ("stopped", cli::WaitFor::Stopped),
        ] {
            let parsed = Cli::try_parse_from(["clank", "wait", "--for", raw]).unwrap();
            let Command::Wait(w) = parsed.command else {
                panic!("expected the wait command for --for {raw}");
            };
            assert_eq!(w.r#for, Some(want));
        }
        assert!(Cli::try_parse_from(["clank", "wait", "--for", "commit", "--peek"]).is_err());
        assert!(
            Cli::try_parse_from(["clank", "wait", "--for", "commit", "--author", "bob"]).is_ok()
        );
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
