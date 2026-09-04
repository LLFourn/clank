//! The command line: the clap tree, its dispatch, and the README's
//! checkable claims. In the lib, not the bin, so those claims are
//! tested where every other test is — the bin has no test target of
//! its own (the-test-suite-takes-a-million-years).

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "clank",
    version,
    about = "Multi-agent peer review around plans"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    // ── getting set up ──
    /// Scaffold `.clank/` in a repo
    ///
    /// Creates `plans/` and the managed `.gitignore`, and installs the
    /// `post-rewrite` hook. `--team <name>` seeds the roster from a
    /// saved template.
    Init(crate::cli::InitArgs),
    /// Install skills and hooks into ~/.claude, ~/.codex, ~/.grok
    ///
    /// User-scope assets: the role-split skill files plus Stop-hook
    /// entries for the tools with active hooks (claude, codex).
    /// Re-run after upgrading; `--force` refreshes edited skills.
    Setup(crate::cli::SetupArgs),
    /// Check the clank setup across repo, user, and session
    ///
    /// Per-section OK / WARN / FAIL with actionable messages. Exits 0
    /// if everything is OK or WARN; 1 on any FAIL.
    Doctor(crate::cli::DoctorArgs),

    // ── the daily flow ──
    /// Show the repo's clank state
    ///
    /// HEAD, plans, phases, and the roster. `--watch` renders live,
    /// `--tui` is the full-screen pane, `-j` emits JSON.
    Status(crate::cli::StatusArgs),
    /// Block until this agent has actionable work
    ///
    /// Author and role come from the session binding (`clank as`).
    /// `--peek` probes without blocking; `--for` observes a repo
    /// instead; `--event` adds ad-hoc wake sources (github repos,
    /// commands) alongside the agent config's `wait_events`.
    Wait(crate::cli::WaitArgs),
    /// Manage the plan queue
    ///
    /// `queue add` consumes a draft from `.clank/drafts/`; `promote`
    /// activates the next plan; `remove` / `reprioritise` curate.
    Queue(crate::cli::QueueArgs),
    /// Read or write review feedback
    ///
    /// A typed surface over the on-disk feedback files: `feedback
    /// write --commit <sha> --verdict … -m "…"` and `feedback read`.
    Feedback(crate::cli::FeedbackArgs),
    /// The github event inbox: list, ack, show
    ///
    /// Every github event a wait ingests lands here and re-wakes the
    /// agent until acked. `list` shows the unhandled backlog, `ack`
    /// closes items out, `show` prints one full record.
    Events(crate::cli::EventsArgs),
    /// Print the repo timeline: commits, reviews, github events
    ///
    /// Chronological and newest-first, with github events interleaved
    /// by their github-side time (`--no-github` to leave them out,
    /// `--plan` to scope to one plan, `--oneline` / `-j` for compact
    /// and machine forms).
    Log(crate::cli::LogArgs),
    /// Finalize a plan into `finished/`
    ///
    /// Runs when the gate says FINISHED; `-m` is required (what
    /// changed, then why). With `finish.autosquash` the plan's
    /// commits collapse into one.
    Finish(crate::cli::FinishArgs),
    /// Move a finalized plan back to active
    Unfinish(crate::cli::UnfinishArgs),

    // ── the roster & workspaces ──
    /// Manage this repo's agent roster
    ///
    /// `agent add` / `promote` / `remove` / `list` / `set-review` /
    /// `start`. Each agent carries its tool + launch profile and
    /// role; `agent add <name>` pulls from the global library, or
    /// `--tool` defines one inline.
    Agent(crate::cli::AgentArgs),
    /// Save and reuse roster templates across repos
    ///
    /// `team save` captures this repo's roster; `team list` / `show`
    /// / `delete` manage the library. Seed a repo with
    /// `clank init --team <name>`.
    Team(crate::cli::TeamArgs),
    /// Bind this session to a roster label
    ///
    /// Reads the tool's session id from the environment
    /// (CLAUDE_CODE_SESSION_ID / CODEX_THREAD_ID) and records the
    /// binding in the agent's local config.
    As(crate::cli::AsArgs),
    /// Manage this agent's auto-mode
    ///
    /// `auto on|off|status`, with an optional role designation; the
    /// Stop hook reads it to drive the work loop.
    Auto(crate::cli::AutoArgs),
    /// Open the team workspace in zellij
    ///
    /// Inside a live session, a new tab; outside one, attach to (or
    /// create) the repo's session. `open dry <path>` classifies
    /// without opening.
    Open(crate::cli::OpenArgs),
    /// Fork the team into a linked worktree
    ///
    /// Creates the worktree with the whole team's sessions forked
    /// into it (opens a tab when inside zellij).
    Fork(crate::cli::ForkArgs),

    // ── plan surgery ──
    /// Set a plan's commits aside and restore them later
    ///
    /// git-stash verbs: `stash push` / `pop` / `show` / `drop`; bare
    /// `clank stash` lists. `push --to-queue` re-queues the plan body
    /// for a later attempt.
    Stash(crate::cli::StashArgs),
    /// Hidden alias for `clank stash push` (+ `shelve clean` →
    /// `stash drop`). One release of back-compat.
    #[command(hide = true)]
    Shelve(crate::cli::ShelveArgs),
    /// Hidden alias for `clank stash pop`. One release of back-compat.
    #[command(hide = true)]
    Unshelve(crate::cli::UnshelveArgs),
    /// Copy plans from another branch
    ///
    /// Brings their commits + plan files onto the current branch; the
    /// source is never modified. Run it where the plans should land.
    Pick(crate::cli::PickArgs),
    /// Strip a plan's `.clank/` artifacts from history
    ///
    /// `--drop` removes the plan entirely (commits included).
    Purge(crate::cli::PurgeArgs),
    /// Run the review loop against a GitHub PR
    ///
    /// The team iterates on a shared pending review until it agrees,
    /// then submits it as one published review.
    PrReview(crate::cli::PrReviewArgs),

    // ── odds & ends ──
    /// Open a plan or commit-range diff in your editor
    Diff(crate::cli::DiffArgs),
    /// Render the timeline + status to a static site
    ///
    /// Writes `.clank/html/`; `clank html open` builds and launches
    /// it in your browser.
    Html(crate::cli::HtmlArgs),
    /// Ask the human a blocking question
    Block(crate::cli::BlockArgs),
    /// Answer a pending block
    Unblock(crate::cli::UnblockArgs),
    /// Record the background task you are waiting on, so the Stop hook
    /// stops waking you with work you are already doing
    Attending(crate::cli::AttendingArgs),
    /// Run a command under clank, so it knows exactly when the work ends
    ///
    /// Background this instead of the bare command:
    /// `clank run --desc "test run" -- cargo test`. Clank records what
    /// it is attending, then BECOMES the command — same pid, same
    /// stdio, same exit status — so the harness task's lifetime is the
    /// work's lifetime and nothing has to be told when it finished.
    Run(crate::cli::RunArgs),
    /// Read or write clank config values
    Config(crate::cli::ConfigArgs),
    /// Dump the repo's roster config as JSON
    ///
    /// Pretty-printed to stdout; fails closed on a pre-roster config
    /// shape with a re-init hint.
    Export(crate::cli::ExportArgs),
    /// Re-open review on a plan's latest reviewable commit
    ///
    /// Rewrites it with the same tree and message so it becomes a new
    /// commit that every reviewer owes a fresh verdict on. Use after a
    /// promotion auto-continued a demoted master's self-review.
    Rereview(crate::cli::RereviewArgs),
    /// Remap feedback after a rebase or amend
    ///
    /// Installed as the `post-rewrite` git hook by `clank init`;
    /// reads old→new SHA pairs from stdin with `--from-stdin`.
    Rewire(crate::cli::RewireArgs),
    /// The per-tool Stop-hook adapter
    ///
    /// Reads HookInput JSON on stdin and emits the tool's
    /// continuation output. Installed by `clank setup`; not typically
    /// invoked directly.
    StopHook(crate::cli::StopHookArgs),
}

/// Run the parsed command. The bin's `main` owns process concerns —
/// SIGPIPE, tracing, the exit code — and calls this for the rest.
pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Init(args) => crate::cli::init::run(args).await,
        Command::Finish(args) => crate::cli::finish::run(args).await,
        Command::Unfinish(args) => crate::cli::unfinish::run(args).await,
        Command::Queue(args) => crate::cli::queue::run(args).await,
        Command::Pick(args) => crate::cli::pick::run(args).await,
        Command::Agent(args) => crate::cli::agent::run(args).await,
        Command::Team(args) => crate::cli::team::run(args).await,
        Command::Export(args) => crate::cli::export::run(args).await,
        Command::Purge(args) => crate::cli::purge::run(args).await,
        Command::Diff(args) => crate::cli::diff::run(args).await,
        Command::Stash(args) => crate::cli::stash::run(args).await,
        Command::Shelve(args) => crate::cli::stash::run_shelve_alias(args).await,
        Command::Unshelve(args) => crate::cli::stash::run_unshelve_alias(args).await,
        Command::Fork(args) => crate::cli::fork::run(args).await,
        Command::PrReview(args) => crate::cli::pr_review::run(args).await,
        Command::Log(args) => crate::cli::log::run(args).await,
        Command::Html(args) => crate::cli::html::run(args).await,
        Command::Open(args) => crate::cli::open::run(args).await,
        Command::Rereview(args) => crate::cli::rereview::run(args).await,
        Command::Rewire(args) => crate::cli::rewire::run(args).await,
        Command::Status(args) => crate::cli::status::run(args).await,
        Command::Wait(args) => crate::cli::wait::run(args).await,
        Command::Feedback(args) => crate::cli::feedback::run(args).await,
        Command::Events(args) => crate::cli::events::run(args).await,
        Command::As(args) => crate::cli::as_cmd::run(args).await,
        Command::Auto(args) => crate::cli::auto::run(args).await,
        Command::StopHook(args) => crate::cli::stop_hook::run(args).await,
        Command::Setup(args) => crate::cli::setup::run(args).await,
        Command::Doctor(args) => crate::cli::doctor::run(args).await,
        Command::Config(args) => crate::cli::config::run(args).await,
        Command::Block(args) => crate::cli::block::run(args).await,
        Command::Unblock(args) => crate::cli::block::run_unblock(args).await,
        Command::Attending(args) => crate::cli::attending::run(args).await,
        Command::Run(args) => crate::cli::run::run(args),
    }
}

/// Map error variants to exit codes. Defaults to 1. `wait` no longer
/// has an expected non-zero exit: it parks until a wake, a real
/// error, or owner death (remove-wait-timeout), so exit 2 is free.
pub fn exit_code_for(err: &anyhow::Error) -> i32 {
    if err
        .downcast_ref::<crate::cli::doctor::DoctorFailed>()
        .is_some()
    {
        return 1;
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The README's checkable claims, asserted against clap itself.
    ///
    /// Beside `Cli` on purpose: a test that restated the command list
    /// would be a second source of truth that goes stale silently — a
    /// command deleted from `Cli` would leave both the README and the
    /// copy untouched and the test green (codex on fa63541).
    fn readme() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("README.md");
        std::fs::read_to_string(&path).expect("README.md at the workspace root")
    }

    /// Every clank subcommand the README names is real.
    ///
    /// Covers BOTH shapes a reader meets: the reference table's first
    /// column, including the `a / b` cells, and every `clank <sub>`
    /// someone would actually type. Scanning only the table missed the
    /// Quickstart entirely, which is where the commands that matter
    /// are.
    #[test]
    fn the_readme_names_only_real_subcommands() {
        use clap::CommandFactory;
        let cmd = Cli::command();
        let real: Vec<String> = cmd
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        let known = |n: &str| real.iter().any(|r| r == n);

        let text = readme();
        let mut unknown: Vec<String> = Vec::new();

        // Shape 1: reference-table rows, `| `name` | …` — and cells
        // that name two commands, `block` / `unblock`.
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("| `")
                && let Some(cell) = rest.split(" |").next()
            {
                for name in cell.split(" / ") {
                    let name = name.trim().trim_matches('`');
                    if !name.is_empty() && !known(name) {
                        unknown.push(format!("table: {name}"));
                    }
                }
            }
        }

        // Shape 2: every `clank <sub>` inside code — fenced blocks and
        // inline spans. Prose is excluded deliberately: "the clank
        // opencode plugin" is a sentence, not an invocation.
        //
        // Scanned per LINE. Scanning the joined text let `cd clank`
        // at the end of one line pair with `cargo` at the start of the
        // next and report a command nobody wrote.
        let mut code: Vec<String> = Vec::new();
        let mut fenced = false;
        for line in text.lines() {
            if line.starts_with("```") {
                fenced = !fenced;
                continue;
            }
            if fenced {
                code.push(line.to_string());
            } else {
                for (i, span) in line.split('`').enumerate() {
                    if i % 2 == 1 {
                        code.push(span.to_string());
                    }
                }
            }
        }
        for line in &code {
            for pair in line.split_whitespace().collect::<Vec<_>>().windows(2) {
                if pair[0] == "clank"
                    && pair[1]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_lowercase())
                    && !known(pair[1])
                {
                    unknown.push(format!("invocation: clank {}", pair[1]));
                }
            }
        }

        assert!(
            unknown.is_empty(),
            "README names commands that do not exist: {unknown:?}"
        );
    }

    /// The README must tell a newcomer how to START the agents.
    ///
    /// The defect the rewrite existed to fix: the Quickstart built a
    /// roster, bound sessions and wrote verdicts without ever
    /// launching anything, so following it produced nothing running.
    #[test]
    fn the_readme_quickstart_launches_the_agents() {
        let text = readme();
        let quickstart = text
            .split("## Quickstart")
            .nth(1)
            .expect("a Quickstart section")
            .split("\n## ")
            .next()
            .unwrap()
            .to_string();
        assert!(
            quickstart.contains("clank open"),
            "the Quickstart must contain the step that starts the agents"
        );
    }

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
            ("commit", crate::cli::WaitFor::Commit),
            ("finished", crate::cli::WaitFor::Finished),
            ("stopped", crate::cli::WaitFor::Stopped),
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
        use crate::cli::html::{HtmlOpenTarget, html_open_argv};
        use crate::cli::{HtmlCmd, HtmlOpenArgs};
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
